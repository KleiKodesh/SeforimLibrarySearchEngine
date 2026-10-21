//! The text scanner: FtsLib's `HtmlWordScanner.cs` HTML layer with Otzaria's
//! word rule (`hebrew_tokenizer.rs`), which FtsLib follows too.
//!
//! Single-pass HTML-aware word scanner. Handles tag detection, entity skipping
//! and Otzaria's word boundaries, normalization and paired readings. Every
//! word is reported with its raw source span (byte AND UTF-16 offsets into the
//! original string), its normalized form and its position.
//!
//! The C# original indexes strings by UTF-16 code units; this port walks the
//! UTF-8 bytes carrying both byte and UTF-16 offsets, emitting byte offsets
//! (natural for Rust slicing / tantivy) alongside UTF-16 offsets (what the JS
//! frontend and the C# descriptors use). `scanner_reference.rs` computes the
//! same output with Otzaria's own functions; a property test keeps them equal.

use crate::chars::{self, is_quote, is_transparent, is_word_char, starts_word};
use crate::html_tags;
use crate::word_rule::{is_reading_suffix, Normalized, Normalizer};

/// Maximum number of chars accumulated for a tag name (as in the C# original).
const TAG_NAME_CAP: usize = 16;

/// A word's location, reported to the scan callback. The normalized form is
/// passed separately as `&str`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WordSpan {
    /// Byte index of the first char of the word in the original string.
    pub byte_start: usize,
    /// Byte index where the next visible element after the word starts (the
    /// separator, a quote or transparent char left out of the word, an
    /// entity's `&`, a block tag's `>`, or the end of the text): inline tags
    /// right after the word are inside its span.
    pub byte_end: usize,
    /// Same span in UTF-16 code units (C#/JS string indices).
    pub utf16_start: usize,
    /// UTF-16 end, exclusive.
    pub utf16_end: usize,
    /// Visible width of the text before the word (chars outside tags in
    /// UTF-16 units, an entity 1, word marks 0). Used by the snippet window
    /// locator to measure window length without re-scanning.
    pub visible_start: usize,
    /// UTF-16 length of the word's quote-free, mark-free term (FtsLib's term,
    /// its `NormLen`): the word's visible width.
    pub letters: usize,
    /// UTF-8 bytes of the term before lowercasing, quotes included: what
    /// Otzaria's `RemoveLongFilter` measures. At [`chars::MAX_TERM_BYTES`]
    /// or more the word is not indexed (it keeps its position).
    pub term_bytes: usize,
    /// The word's position: one per word, except the second reading of a
    /// paired reading `(X) [Y]`, which takes X's position (Otzaria).
    pub position: usize,
}

/// Scans `text` and calls `on_word(normalized, span)` for each word, in order.
pub fn scan(text: &str, on_word: impl FnMut(&str, WordSpan)) -> usize {
    scan_with(text, false, on_word)
}

/// A quote or transparent char after a word, not yet known to belong to it.
#[derive(Clone, Copy)]
struct PendingChar {
    c: char,
    byte: usize,
    utf16: usize,
}

/// After a word X preceded by `(`: how far the text matches `) [` (Otzaria
/// `paired_reading_after`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gap {
    None,
    /// reading suffix chars, then `)`
    Suffix,
    /// whitespace, then `[`
    Space,
    /// right after `[`: the next element must start Y
    Open,
}

/// Word state of the scanner.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Outside,
    InWord,
    /// a word followed by quotes / transparent chars (in `pending`)
    Pending,
}

struct Scan<'f, F: FnMut(&str, WordSpan)> {
    on_word: &'f mut F,
    normalizer: Normalizer,
    norm: Normalized,
    state: State,
    start_byte: usize,
    start_utf16: usize,
    start_visible: usize,
    pending: Vec<PendingChar>,
    geresh_n: usize,
    gershayim_n: usize,
    first_quote: usize,
    first_is_geresh: bool,
    /// char of the last visible element (a break is a space)
    prev: char,
    /// the current word follows `(`
    after_paren: bool,
    /// the current word follows `) [` of a word in `(`: a second reading
    second_reading: bool,
    gap: Gap,
    /// a second reading waiting for its `]`
    held: Option<(String, WordSpan)>,
    words: usize,
}

impl<F: FnMut(&str, WordSpan)> Scan<'_, F> {
    /// Feeds a visible element outside any word to the paired-reading checks.
    #[inline]
    fn after_word(&mut self, c: char) {
        if self.held.is_some() {
            if is_reading_suffix(c) {
                return;
            }
            self.release(c == ']');
        }
        self.gap = match (self.gap, c) {
            (Gap::None, _) => Gap::None,
            (Gap::Suffix, ')') => Gap::Space,
            (Gap::Suffix, c) if is_reading_suffix(c) => Gap::Suffix,
            (Gap::Space, '[') => Gap::Open,
            (Gap::Space, c) if c.is_whitespace() => Gap::Space,
            _ => Gap::None,
        };
    }

    /// Emits the held second reading: at its first reading's position when
    /// its `]` came, else at its own.
    fn release(&mut self, paired: bool) {
        if let Some((text, mut span)) = self.held.take() {
            span.position = if paired {
                self.words - 1
            } else {
                self.words += 1;
                self.words - 1
            };
            (self.on_word)(&text, span);
        }
    }

    /// An element outside a word: the paired-reading checks, then maybe a new
    /// word.
    #[inline]
    fn outside(&mut self, c: char, byte: usize, utf16: usize, visible: usize) {
        let open = self.gap == Gap::Open;
        self.after_word(c);
        if starts_word(c) {
            self.state = State::InWord;
            self.start_byte = byte;
            self.start_utf16 = utf16;
            self.start_visible = visible;
            self.after_paren = self.prev == '(';
            self.second_reading = open;
            self.gap = Gap::None;
            self.norm.text.clear();
            self.norm.term_bytes = 0;
            self.norm.letters = 0;
            self.normalizer.push(&mut self.norm, c);
        }
    }

    /// Ends the current word at raw `(byte, utf16)`.
    fn finish(&mut self, byte: usize, utf16: usize) {
        self.state = State::Outside;
        let span = WordSpan {
            byte_start: self.start_byte,
            byte_end: byte,
            utf16_start: self.start_utf16,
            utf16_end: utf16,
            visible_start: self.start_visible,
            letters: self.norm.letters,
            term_bytes: self.norm.term_bytes,
            position: 0,
        };
        if self.second_reading {
            self.held = Some((self.norm.text.clone(), span));
        } else {
            self.words += 1;
            (self.on_word)(&self.norm.text, WordSpan { position: self.words - 1, ..span });
        }
        self.gap = if self.after_paren { Gap::Suffix } else { Gap::None };
    }

    /// A non-word element (or the end of the text, `c` = None) after the
    /// current word: ends it per Otzaria (a trailing geresh is kept).
    fn end_word(&mut self, at_byte: usize, at_utf16: usize) {
        match self.state {
            State::Outside => {}
            State::InWord => self.finish(at_byte, at_utf16),
            State::Pending => {
                let from = if self.first_is_geresh {
                    let q = self.pending[self.first_quote];
                    self.normalizer.push(&mut self.norm, q.c);
                    self.first_quote + 1
                } else {
                    0
                };
                let (b, u) = self.pending.get(from).map_or((at_byte, at_utf16), |p| (p.byte, p.utf16));
                self.finish(b, u);
                self.replay(from);
            }
        }
    }

    /// Feeds the pending chars from `from` on (left out of the word) to the
    /// paired-reading checks.
    fn replay(&mut self, from: usize) {
        for k in from..self.pending.len() {
            let c = self.pending[k].c;
            self.after_word(c);
        }
    }

    /// One visible char (a break is `' '` with `is_break`).
    #[inline]
    fn element(&mut self, c: char, byte: usize, utf16: usize, visible: usize) {
        match self.state {
            State::Outside => self.outside(c, byte, utf16, visible),
            State::InWord => {
                if is_word_char(c) {
                    self.normalizer.push(&mut self.norm, c);
                } else if is_quote(c) || is_transparent(c) {
                    self.pending.clear();
                    self.geresh_n = 0;
                    self.gershayim_n = 0;
                    self.first_is_geresh = false;
                    self.state = State::Pending;
                    self.pend(c, byte, utf16);
                } else {
                    self.finish(byte, utf16);
                    self.outside(c, byte, utf16, visible);
                }
            }
            State::Pending => {
                if is_quote(c) || is_transparent(c) {
                    self.pend(c, byte, utf16);
                } else if is_word_char(c) {
                    if matches!((self.geresh_n, self.gershayim_n), (0, 0) | (1, 0) | (2, 0) | (0, 1)) {
                        for k in 0..self.pending.len() {
                            let p = self.pending[k].c;
                            self.normalizer.push(&mut self.norm, p);
                        }
                        self.normalizer.push(&mut self.norm, c);
                        self.state = State::InWord;
                    } else {
                        let first = self.pending[0];
                        self.finish(first.byte, first.utf16);
                        self.replay(0);
                        self.outside(c, byte, utf16, visible);
                    }
                } else {
                    self.end_word(byte, utf16);
                    self.outside(c, byte, utf16, visible);
                }
            }
        }
        self.prev = c;
    }

    #[inline]
    fn pend(&mut self, c: char, byte: usize, utf16: usize) {
        if is_quote(c) {
            if self.geresh_n + self.gershayim_n == 0 {
                self.first_quote = self.pending.len();
                self.first_is_geresh = chars::is_geresh(c);
            }
            if chars::is_geresh(c) {
                self.geresh_n += 1;
            } else {
                self.gershayim_n += 1;
            }
        }
        self.pending.push(PendingChar { c, byte, utf16 });
    }
}

/// [`scan`], keeping the word marks (nikud, te'amim) in the normalized text
/// when `keep_marks` (the marked index flavour).
///
/// Rules (Otzaria's tokenizer; FtsLib follows it):
/// - A word starts at a letter or digit of any script (Hebrew, ASCII, and
///   every other alphanumeric char; no Hebrew/Latin split) and runs over
///   letters, digits and word marks. It has any length, one letter included.
/// - Transparent chars (`. [ ] * ~ ^ $ \ +`, backtick, zero-width and bidi
///   controls) between word chars join them and are dropped. A run of quotes
///   between word chars joins them when it is one geresh (kept as `'`), two
///   gereshes or one gershayim (kept as `"`); typographic quotes count as
///   geresh / gershayim. After a word a geresh is kept (`'`), a gershayim
///   is not.
/// - Hebrew presentation forms fold to their letters; marks are dropped
///   unless kept; the term is lowercased (every script).
/// - The second reading of `(X) [Y]` takes X's position.
/// - HTML (FtsLib): inline tags are transparent (a word continues across
///   them); block tags and `sup`/`sub` end the word; every entity (or a bare
///   `&`) is a separator kept outside the word's span.
/// - A word whose term has [`chars::MAX_TERM_BYTES`] bytes or more is still
///   reported (it is a word for positions and distances); the index
///   tokenizer leaves it out of the terms.
pub fn scan_with(text: &str, keep_marks: bool, mut on_word: impl FnMut(&str, WordSpan)) -> usize {
    let bytes = text.as_bytes();
    let n = bytes.len();
    if n == 0 {
        return 0;
    }
    let mut s = Scan {
        on_word: &mut on_word,
        normalizer: Normalizer { keep_marks, wildcards: false },
        norm: Normalized { text: String::with_capacity(64), ..Normalized::default() },
        state: State::Outside,
        start_byte: 0,
        start_utf16: 0,
        start_visible: 0,
        pending: Vec::new(),
        geresh_n: 0,
        gershayim_n: 0,
        first_quote: 0,
        first_is_geresh: false,
        prev: ' ',
        after_paren: false,
        second_reading: false,
        gap: Gap::None,
        held: None,
        words: 0,
    };

    let mut tag_name = ['\0'; TAG_NAME_CAP];
    let mut tag_len = 0usize;
    let mut in_tag = false;
    let mut tag_name_done = false;
    let mut visible = 0usize;
    let mut b = 0usize;
    let mut u = 0usize;

    while b < n {
        if !in_tag {
            let x = bytes[b];
            // Fast path: a Hebrew letter (U+05D0..U+05EA) inside a word.
            if x == 0xD7 && (0x90..=0xAA).contains(&bytes[b + 1]) && s.state == State::InWord {
                s.norm.text.push_str(&text[b..b + 2]);
                s.norm.term_bytes += 2;
                s.norm.letters += 1;
                s.prev = '\u{05D0}';
                visible += 1;
                b += 2;
                u += 1;
                continue;
            }
            // Fast path: a Hebrew letter starting a word, no paired-reading
            // check open (`outside` with nothing to feed).
            if x == 0xD7
                && (0x90..=0xAA).contains(&bytes[b + 1])
                && s.state == State::Outside
                && s.gap == Gap::None
                && s.held.is_none()
            {
                s.state = State::InWord;
                s.start_byte = b;
                s.start_utf16 = u;
                s.start_visible = visible;
                s.after_paren = s.prev == '(';
                s.second_reading = false;
                s.norm.text.clear();
                s.norm.text.push_str(&text[b..b + 2]);
                s.norm.term_bytes = 2;
                s.norm.letters = 1;
                s.prev = '\u{05D0}';
                visible += 1;
                b += 2;
                u += 1;
                continue;
            }
            // Fast path: a space ending a word, or between words, no
            // paired-reading check open.
            if x == b' ' && s.held.is_none() {
                if s.state == State::InWord && !s.after_paren && !s.second_reading {
                    s.finish(b, u); // leaves the gap check closed: no '(' before the word
                }
                if s.state == State::Outside && s.gap == Gap::None {
                    s.prev = ' ';
                    visible += 1;
                    b += 1;
                    u += 1;
                    continue;
                }
            }
            // Fast path: a Hebrew point or ta'am inside a word (U+0591..U+05BD,
            // U+05BF, U+05C1, U+05C2, U+05C4, U+05C5, U+05C7).
            if s.state == State::InWord
                && ((x == 0xD6 && bytes[b + 1] >= 0x91 && bytes[b + 1] != 0xBE)
                    || (x == 0xD7 && matches!(bytes[b + 1], 0x81 | 0x82 | 0x84 | 0x85 | 0x87)))
            {
                if keep_marks {
                    s.norm.text.push_str(&text[b..b + 2]);
                    s.norm.term_bytes += 2;
                }
                s.prev = '\u{05B0}';
                b += 2;
                u += 1;
                continue;
            }
        }

        let (c, clen) = decode_char(bytes, b);
        let ulen = if clen == 4 { 2 } else { 1 };

        if in_tag {
            if c == '>' {
                if html_tags::is_word_break_tag(&tag_name[..tag_len]) {
                    s.element(' ', b, u, visible);
                }
                in_tag = false;
                tag_len = 0;
            } else if !tag_name_done {
                if c == ' ' || c == '\t' {
                    tag_name_done = true;
                } else if tag_len < TAG_NAME_CAP && c != '/' {
                    tag_name[tag_len] = c;
                    tag_len += 1;
                }
            } else {
                // Attributes: nothing matters until the closing '>'.
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
            // A tag: inline tags are transparent (the word continues across
            // them); the decision is taken at '>' once the name is known.
            in_tag = true;
            tag_len = 0;
            tag_name_done = false;
            b += 1;
            u += 1;
            continue;
        }

        if c == '&' {
            // Every entity (or a bare '&') is a separator at its '&'.
            s.element(' ', b, u, visible);
            skip_entity(bytes, &mut b, &mut u);
            visible += 1;
            b += 1;
            u += 1;
            continue;
        }

        s.element(c, b, u, visible);
        if !chars::is_word_mark(c) {
            visible += ulen;
        }
        b += clen;
        u += ulen;
    }

    s.end_word(n, u);
    s.release(false);
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
    /// Byte index of the first char of the word in the original raw string.
    pub raw_start: usize,
    /// Byte index of the next visible element after the word
    /// ([`WordSpan::byte_end`]).
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
    /// The word's position ([`WordSpan::position`]): the index positions.
    pub position: usize,
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
                position: span.position,
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

    fn positions(text: &str) -> Vec<(String, usize)> {
        spans(text).into_iter().map(|(w, s)| (w, s.position)).collect()
    }

    const ALEF: &str = "\u{05D0}";
    const BET: &str = "\u{05D1}";
    const GIMEL: &str = "\u{05D2}";
    const DALET: &str = "\u{05D3}";
    const HE: &str = "\u{05D4}";
    const VAV: &str = "\u{05D5}";

    fn heb(parts: &[&str]) -> String {
        parts.concat()
    }

    #[test]
    fn plain_words() {
        let a = heb(&[ALEF, BET]);
        let b = heb(&[GIMEL, DALET]);
        assert_eq!(words(&format!("{a} {b}")), [a, b]);
    }

    #[test]
    fn nikud_stripped_or_kept() {
        let text = "\u{05D1}\u{05B0}\u{05BC}\u{05E8}\u{05B5}\u{05D0}\u{05E9}\u{05B4}\u{05C1}\u{05D9}\u{05EA} \u{05D1}\u{05B8}\u{05BC}\u{05E8}\u{05B8}\u{05D0}";
        assert_eq!(words(text), ["\u{05D1}\u{05E8}\u{05D0}\u{05E9}\u{05D9}\u{05EA}", "\u{05D1}\u{05E8}\u{05D0}"]);
        let mut out = Vec::new();
        scan_with(text, true, |w, _| out.push(w.to_string()));
        assert_eq!(out, ["\u{05D1}\u{05B0}\u{05BC}\u{05E8}\u{05B5}\u{05D0}\u{05E9}\u{05B4}\u{05C1}\u{05D9}\u{05EA}", "\u{05D1}\u{05B8}\u{05BC}\u{05E8}\u{05B8}\u{05D0}"]);
    }

    #[test]
    fn rule1_period_brackets_bidi_join() {
        // letter.letter.letter: one word
        assert_eq!(words(&heb(&[ALEF, ".", BET, ".", GIMEL])), [heb(&[ALEF, BET, GIMEL])]);
        // a period after a word is not part of it
        let s = spans(&heb(&[ALEF, BET, ". ", GIMEL]));
        assert_eq!(s[0].0, heb(&[ALEF, BET]));
        assert_eq!((s[0].1.utf16_end, s[1].0.as_str()), (2, GIMEL));
        // brackets join (letter[letter]letter)
        assert_eq!(words(&heb(&[ALEF, "[", BET, "]", GIMEL])), [heb(&[ALEF, BET, GIMEL])]);
        // bidi and zero-width marks join
        assert_eq!(words(&heb(&[ALEF, "\u{200F}", BET, "\u{200B}", GIMEL, "\u{202B}", DALET])), [heb(&[ALEF, BET, GIMEL, DALET])]);
        // other transparent chars: * ~ ^ $ \ + backtick
        assert_eq!(words(&heb(&[ALEF, "*", BET, "~", GIMEL, "^$\\+`", DALET])), [heb(&[ALEF, BET, GIMEL, DALET])]);
        // FtsLib separators stay separators
        assert_eq!(words(&heb(&[ALEF, ",", BET, "-", GIMEL, "\u{05BE}", DALET, "?", HE])), [ALEF, BET, GIMEL, DALET, HE]);
    }

    #[test]
    fn rule2_no_script_split_other_scripts() {
        assert_eq!(words(&heb(&["abc", DALET, HE])), [heb(&["abc", DALET, HE])]);
        assert_eq!(words(&heb(&[DALET, HE, "ABC"])), [heb(&[DALET, HE, "abc"])]);
        // accented Latin, Greek, Cyrillic: word chars, lowercased
        assert_eq!(words("Caf\u{00C9} \u{0391}\u{03B2} \u{0416}\u{0436}"), ["caf\u{00E9}", "\u{03B1}\u{03B2}", "\u{0436}\u{0436}"]);
        // a combining accent continues a word but never starts one
        assert_eq!(words("e\u{0301}x \u{0301}y"), ["ex", "y"]);
        assert_eq!(words("5ab a5\u{05D1} 3.14"), ["5ab", "a5\u{05D1}", "314"]);
    }

    #[test]
    fn rule3_quotes_and_trailing_geresh() {
        // inner quotes: one geresh ', two gereshes or one gershayim "
        assert_eq!(words(&heb(&[ALEF, "'", BET])), [heb(&[ALEF, "'", BET])]);
        assert_eq!(words(&heb(&[ALEF, "''", BET])), [heb(&[ALEF, "\"", BET])]);
        assert_eq!(words(&heb(&[ALEF, "\u{05F4}", BET])), [heb(&[ALEF, "\"", BET])]);
        // a run of other quotes splits
        assert_eq!(words(&heb(&[ALEF, "'\"", BET])), [ALEF.to_string(), BET.to_string()]);
        // trailing geresh kept (with its span), trailing gershayim not
        let s = spans(&heb(&[ALEF, BET, "' ", GIMEL, DALET, "\" ", HE]));
        assert_eq!(s[0].0, heb(&[ALEF, BET, "'"]));
        assert_eq!(s[0].1.utf16_end, 3);
        assert_eq!(s[1].0, heb(&[GIMEL, DALET]));
        assert_eq!(s[1].1.utf16_end, 6);
        // only the first geresh of a trailing run, period between allowed
        assert_eq!(words(&heb(&[ALEF, BET, ".''"])), [heb(&[ALEF, BET, "'"])]);
        // a leading quote is not part of a word
        assert_eq!(words(&heb(&["\"", ALEF, BET, "\""])), [heb(&[ALEF, BET])]);
    }

    #[test]
    fn rule4_typographic_quotes_and_presentation_forms() {
        assert_eq!(words(&heb(&[ALEF, "\u{201D}", BET])), [heb(&[ALEF, "\"", BET])]);
        assert_eq!(words(&heb(&[ALEF, "\u{201C}", BET])), [heb(&[ALEF, "\"", BET])]);
        assert_eq!(words(&heb(&[ALEF, "\u{2019}", BET, " ", GIMEL, "\u{2018}"])), [heb(&[ALEF, "'", BET]), heb(&[GIMEL, "'"])]);
        assert_eq!(words(&heb(&[ALEF, "\u{2019}\u{2019}", BET])), [heb(&[ALEF, "\"", BET])]);
        // presentation forms fold (marks dropped unless kept)
        assert_eq!(words("\u{FB2B}\u{05DC}\u{05D5}\u{05DD} \u{FB4F} \u{FB31}"), ["\u{05E9}\u{05DC}\u{05D5}\u{05DD}", "\u{05D0}\u{05DC}", "\u{05D1}"]);
        let mut out = Vec::new();
        scan_with("\u{FB31}\u{05D9}", true, |w, _| out.push(w.to_string()));
        assert_eq!(out, ["\u{05D1}\u{05BC}\u{05D9}"]);
    }

    #[test]
    fn rule5_paired_reading_shares_a_position() {
        let x = heb(&[ALEF, BET]);
        let y = heb(&[GIMEL, DALET]);
        let got = positions(&format!("{HE} ({x}) [{y}] {VAV}"));
        assert_eq!(got, [(HE.into(), 0), (x.clone(), 1), (y.clone(), 1), (VAV.into(), 2)]);
        // punctuation inside the brackets, tags around them
        let got = positions(&format!("({x}.) <b>[{y}:]</b> {VAV}"));
        assert_eq!(got, [(x.clone(), 0), (y.clone(), 0), (VAV.into(), 1)]);
        // not a pair: no closing bracket, a word between, no opening paren
        assert_eq!(positions(&format!("({x}) [{y} {VAV}")), [(x.clone(), 0), (y.clone(), 1), (VAV.into(), 2)]);
        assert_eq!(positions(&format!("({x}) {VAV} [{y}]")), [(x.clone(), 0), (VAV.into(), 1), (y.clone(), 2)]);
        assert_eq!(positions(&format!("{x}) [{y}]")), [(x.clone(), 0), (y.clone(), 1)]);
        // at the end of the text
        assert_eq!(positions(&format!("({x}) [{y}]")), [(x.clone(), 0), (y.clone(), 0)]);
    }

    #[test]
    fn rule6_term_bytes() {
        let s = spans(&heb(&[ALEF, "\u{05F4}", BET, " ", &"\u{05D0}".repeat(64)]));
        assert_eq!((s[0].1.term_bytes, s[0].1.letters), (5, 2));
        assert_eq!(s[1].1.term_bytes, 128);
        assert!(!chars::is_indexable_len(s[1].1.term_bytes));
    }

    #[test]
    fn maqaf_and_html() {
        assert_eq!(words("\u{05E2}\u{05DC}\u{05BE}\u{05E4}\u{05E0}\u{05D9}"), ["\u{05E2}\u{05DC}", "\u{05E4}\u{05E0}\u{05D9}"]);
        assert_eq!(words(&heb(&[ALEF, BET, "<b>", GIMEL, "</b> ", DALET])), [heb(&[ALEF, BET, GIMEL]), DALET.to_string()]);
        assert_eq!(words(&heb(&[ALEF, BET, "<br>", GIMEL, DALET])), [heb(&[ALEF, BET]), heb(&[GIMEL, DALET])]);
        assert_eq!(words(&heb(&[ALEF, BET, "<sup>1</sup>", GIMEL])), [heb(&[ALEF, BET]), "1".into(), GIMEL.into()]);
        assert_eq!(words(&heb(&[ALEF, BET, "&nbsp;", GIMEL, "&", DALET])), [heb(&[ALEF, BET]), GIMEL.into(), DALET.into()]);
        let text = heb(&[ALEF, BET, "<b>", GIMEL, "</b> ", DALET]);
        let s = spans(&text);
        assert_eq!(&text[s[0].1.byte_start..s[0].1.byte_end], heb(&[ALEF, BET, "<b>", GIMEL, "</b>"]));
    }

    #[test]
    fn raw_spans_and_visible_counts() {
        let text = "\u{05E9}\u{05B8}\u{05C1}\u{05DC}\u{05D5}\u{05B9}\u{05DD} \u{05E2}\u{05D5}\u{05DC}\u{05DD}";
        let s = spans(text);
        assert_eq!(&text[s[0].1.byte_start..s[0].1.byte_end], "\u{05E9}\u{05B8}\u{05C1}\u{05DC}\u{05D5}\u{05B9}\u{05DD}");
        assert_eq!((s[0].1.utf16_end, s[1].1.utf16_end), (7, 12));
        // marks have no width; an entity is one
        assert_eq!(s[1].1.visible_start, 5);
        let s = spans(&heb(&[ALEF, BET, "&nbsp;", GIMEL]));
        assert_eq!(s[1].1.visible_start, 3);
    }

    #[test]
    fn text_tokens_reuse() {
        let mut tt = TextTokens::default();
        let toks: Vec<_> = tt.tokenize(&heb(&[ALEF, "\"", BET, " ", GIMEL, "\u{05BE}", DALET])).to_vec();
        assert_eq!(toks.len(), 3);
        assert_eq!(tt.norm_of(&toks[0]), heb(&[ALEF, "\"", BET]));
        assert_eq!(toks[2].position, 2);
        let toks2: Vec<_> = tt.tokenize(HE).to_vec();
        assert_eq!(tt.norm_of(&toks2[0]), HE);
    }

    // ---- equality with the reference (Otzaria's own functions) ----

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
            assert_eq!(
                scan_all(new_impl, text, keep),
                scan_all(old_impl, text, keep),
                "keep_marks={keep} text={}",
                text.escape_unicode()
            );
        }
    }

    #[test]
    fn matches_reference_on_edge_cases() {
        let long_heb = "\u{05D0}".repeat(70);
        let cases = [
            "", "\u{05D0}", "&", "&;", "&a;", "<", ">", "<b", "\u{05D0}<b", "\u{05D0}&;\u{05D1}", "\u{05D0}<br />\u{05D1}",
            "\u{05D0}<span\tstyle='a>b'>\u{05D1}", "\u{05D0}<!-- c -->\u{05D1}", "\u{05D0}&abcdefghij;\u{05D1}",
            "\u{05D0}''\u{05D1}", "\u{05D0}'''\u{05D1}", "\u{05D0}'\"\u{05D1}", "\u{05D0}\"\"\u{05D1}", "\u{05D0}.'.\u{05D1}",
            "\u{05D0}'", "\u{05D0}''", "\u{05D0}.'", "\u{05D0}\"'", "\u{05D0}'<b>'</b>", "\u{05D0}'&amp;", "\u{05D0}'<br>",
            "(\u{05D0}) [\u{05D1}]", "(\u{05D0}) [\u{05D1}", "(\u{05D0})[\u{05D1}]", "(\u{05D0}'.) \t[\u{05D1}'']", "((\u{05D0})) [[\u{05D1}]]",
            "(\u{05D0}) [\u{05D1}]\u{05D2}", "(\u{05D0}) [\u{05D1}] (\u{05D2}) [\u{05D3}]", "(\u{05D0}<b>) [</b>\u{05D1}]",
            "(\u{05D0})&nbsp;[\u{05D1}]", "(\u{05D0})<br>[\u{05D1}]", "(\u{05D0}) [\u{05B0}\u{05D1}]", "(\u{05D0}\u{05B0}) [\u{05D1}\u{05B0}]",
            "\u{05D0}\u{05B0}'\u{05B0}\u{05D1}", "\u{05B0}\u{05D0}", "a\u{0301}b", "\u{0301}", "\u{FB1E}\u{05D0}\u{FB1E}",
            "\u{FB2E}\u{FB4F}\u{FB1D}", "\u{0130}stanbul", "\u{212A}elvin", "\u{1D400}\u{1D401}", "x\u{1F600}y", "\u{00B2}\u{00BD}",
            "\u{200F}\u{05D0}\u{200E}", "\u{05D0}\u{200F}\u{05D1}", "\u{05E2}\u{05DC}\u{05BE}\u{05E4}", "\u{05D0}\u{05C0}\u{05D1}\u{05C3}",
            "\u{05D0}\r\n\u{05D1}\t\u{05D2}", "\u{05D0}\u{00A0}\u{05D1}\u{3000}\u{05D2}", &long_heb,
        ];
        for c in cases {
            assert_same(c);
        }
    }

    #[test]
    fn matches_reference_on_generated_strings() {
        let pieces = [
            "\u{05D0}", "\u{05D1}", "\u{05EA}", "\u{05DA}", "a", "Z", "q", "1", "\u{00C9}", "\u{0436}", "\u{0663}", "\u{1D400}", "\u{0130}",
            "\u{05B0}", "\u{05B4}", "\u{0591}", "\u{05BC}", "\u{05C1}", "\u{05C7}", "\u{05C0}", "\u{05C3}", "\u{05C6}", "\u{05BE}",
            "\u{0301}", "\u{FB1E}", "\u{FB2B}", "\u{FB4F}", "\u{FB31}", "\"", "'", "\u{05F3}", "\u{05F4}", "\u{2018}", "\u{2019}",
            "\u{201C}", "\u{201D}", ".", "[", "]", "(", ")", "*", "~", "^", "\\", "`", "\u{200F}", "\u{200B}", "\u{FEFF}", "\u{202B}",
            " ", "  ", "\t", "\n", ",", ":", "?", "-", "<b>", "</b>", "<i>", "<br>", "<p>", "</p>", "<sup>", "</sup>", "<span class=\"x\">",
            "</span>", "<", ">", "&nbsp;", "&amp;", "&#160;", "&", ";", "\u{1F600}", "\u{00A0}",
            "(\u{05D0}\u{05D1}) [\u{05D2}\u{05D3}]", ") [", "\u{05D0}\u{05D1}\u{05D2}\u{05D3}\u{05D4}\u{05D5}\u{05D6}",
        ];
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..40_000 {
            let len = (next() % 40) as usize;
            let mut s = String::new();
            for _ in 0..len {
                s.push_str(pieces[(next() % pieces.len() as u64) as usize]);
            }
            assert_same(&s);
        }
    }
}
