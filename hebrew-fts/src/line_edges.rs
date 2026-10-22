//! A line's content edges for cross-line matching (Otzaria `cross_line.rs`
//! `content_range` / `line_edges`, ported as is): a phrase continues from a
//! line's last CONTENT word into the next line's first content word, skipping
//! a leading enumerator such as `(x)`, `[x]` or `{x}` and a trailing one such
//! as `{x}` or `(49):` (one to five marker chars: Hebrew letters, digits,
//! quotes, `*`). A `(X) [Y]` paired reading is content, never an enumerator.
//!
//! Otzaria reads the rule on its normalized line (markup stripped, attached
//! marks dropped, presentation forms folded); [`line_edges`] builds the same
//! view of the raw line, applies the rule there, and maps the content range
//! back to the scanner's word positions, so the index positions and the
//! locator's tokens agree.

use std::ops::Range;

use crate::chars::{fold_presentation_form, is_attached_mark};
use crate::word_rule::{next_token_boundaries, paired_reading_after};

/// A line's words and the positions of its first and last content words
/// (None: the line has no content word, so no phrase crosses into or out of it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LineEdges {
    /// Word positions in the line (the last position + 1).
    pub words: usize,
    /// Position of the first content word.
    pub first: Option<usize>,
    /// Position of the last content word.
    pub last: Option<usize>,
}

/// The edges of one raw line (see the module docs).
pub fn line_edges(text: &str) -> LineEdges {
    let plain = PlainView::new(text);
    let content = content_range(&plain.text);
    let (raw_start, raw_end) = (plain.raw_at(content.start), plain.raw_at(content.end));
    let mut edges = LineEdges::default();
    crate::scanner::scan_with(text, false, |_, span| {
        edges.words = span.position + 1;
        if content.is_empty() {
            return;
        }
        // a content word starts inside the content range: an enumerator's
        // word starts after the leading one's end or at / after the trailing
        // one's start
        if span.byte_start >= raw_start && span.byte_start < raw_end {
            if edges.first.is_none() {
                edges.first = Some(span.position);
            }
            edges.last = Some(span.position);
        }
    });
    edges
}

/// Otzaria `is_marker_char`.
fn is_marker_char(c: char) -> bool {
    ('\u{05D0}'..='\u{05EA}').contains(&c) || c.is_ascii_digit() || matches!(c, '\'' | '"' | '\u{05F3}' | '\u{05F4}' | '*')
}

/// Otzaria `reading_pair_end`: the end of a `(X) [Y]` pair whose `(` is at `open`.
fn reading_pair_end(s: &str, open: usize) -> Option<usize> {
    let (start, end) = next_token_boundaries(s, open + 1, false)?;
    if start != open + 1 {
        return None;
    }
    paired_reading_after(s, start, end).map(|pair| pair.after_pair)
}

/// Otzaria `leading_marker_len`: byte length of a leading enumerator with the
/// whitespace after it; 0 when the line has none.
fn leading_marker_len(s: &str) -> usize {
    let t = s.trim_start();
    let skipped = s.len() - t.len();
    let mut chars = t.char_indices();
    let close = match chars.next() {
        Some((_, '(')) => ')',
        Some((_, '[')) => ']',
        Some((_, '{')) => '}',
        _ => return 0,
    };
    if close == ')' && reading_pair_end(t, 0).is_some() {
        return 0;
    }
    for (n, (i, c)) in chars.enumerate() {
        if c == close {
            if n == 0 || n > 5 {
                return 0;
            }
            let end = i + c.len_utf8();
            let rest = &t[end..];
            return skipped + end + (rest.len() - rest.trim_start().len());
        }
        if !is_marker_char(c) {
            return 0;
        }
    }
    0
}

/// Otzaria `trailing_marker_start`: where a trailing enumerator begins;
/// `s.len()` when the line has none.
fn trailing_marker_start(s: &str) -> usize {
    let t = s.trim_end_matches(|c: char| c.is_whitespace() || matches!(c, ':' | '.' | ',' | '\u{05C3}'));
    let mut chars = t.char_indices().rev();
    let open = match chars.next() {
        Some((_, ')')) => '(',
        Some((_, ']')) => '[',
        Some((_, '}')) => '{',
        _ => return s.len(),
    };
    for (n, (i, c)) in chars.enumerate() {
        if c == open {
            if n == 0 || n > 5 {
                return s.len();
            }
            // a final `[Y]` can be the second reading of `(X) [Y]`
            if open == '[' {
                if let Some(before_close) = t[..i].trim_end().strip_suffix(')') {
                    if let Some(pair_open) = before_close.rfind('(') {
                        if reading_pair_end(t, pair_open) == Some(t.len()) {
                            return s.len();
                        }
                    }
                }
            }
            return t[..i].trim_end().len();
        }
        if !is_marker_char(c) {
            return s.len();
        }
    }
    s.len()
}

/// Otzaria `content_range`: the line without its leading and trailing
/// enumerators (byte range of the normalized line).
pub fn content_range(plain: &str) -> Range<usize> {
    let start = leading_marker_len(plain);
    start..trailing_marker_start(plain).max(start)
}

/// The normalized line Otzaria reads the edges on, with each byte's raw
/// offset: tags removed (a breaking tag as a space; an unterminated `<` kept),
/// whitespace entities as a space and other `&...;` entities removed (its
/// `strip_html_for_indexing`), attached marks dropped, presentation forms folded.
struct PlainView {
    text: String,
    /// Raw byte offset of each byte of `text`, plus the raw length at the end.
    raw: Vec<usize>,
}

const SPACE_ENTITIES: [&str; 4] = ["&nbsp;", "&thinsp;", "&ensp;", "&emsp;"];

impl PlainView {
    fn new(raw: &str) -> PlainView {
        let mut view = PlainView { text: String::with_capacity(raw.len()), raw: Vec::with_capacity(raw.len() + 1) };
        let bytes = raw.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'<' => {
                    if let Some(close) = raw[i + 1..].find('>') {
                        if crate::line_regex::is_breaking_tag_name(&raw[i + 1..i + 1 + close]) {
                            view.push(' ', i);
                        }
                        i += close + 2;
                        continue;
                    }
                }
                b'&' => {
                    if let Some(e) = SPACE_ENTITIES.iter().find(|e| raw[i..].starts_with(**e)) {
                        view.push(' ', i);
                        i += e.len();
                        continue;
                    }
                    // `&[^;]+;` (a space entity inside it counts as one char)
                    let mut j = i + 1;
                    let mut semi = None;
                    while j < bytes.len() {
                        match bytes[j] {
                            b';' => {
                                semi = Some(j);
                                break;
                            }
                            b'&' => j += SPACE_ENTITIES.iter().find(|e| raw[j..].starts_with(**e)).map_or(1, |e| e.len()),
                            _ => j += 1,
                        }
                    }
                    if let Some(semi) = semi.filter(|&s| s > i + 1) {
                        i = semi + 1;
                        continue;
                    }
                }
                _ => {}
            }
            let c = raw[i..].chars().next().unwrap_or(' ');
            if let Some(folded) = fold_presentation_form(c) {
                for f in folded.chars().filter(|&f| !is_attached_mark(f)) {
                    view.push(f, i);
                }
            } else if !is_attached_mark(c) {
                view.push(c, i);
            }
            i += c.len_utf8();
        }
        view.raw.push(raw.len());
        view
    }

    fn push(&mut self, c: char, raw: usize) {
        self.text.push(c);
        self.raw.extend(std::iter::repeat(raw).take(c.len_utf8()));
    }

    /// The raw offset of plain byte `i` (`text.len()`: the raw length).
    fn raw_at(&self, i: usize) -> usize {
        self.raw[i.min(self.raw.len() - 1)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "\u{05D0}";
    const B: &str = "\u{05D1}";
    const G: &str = "\u{05D2}";
    const D: &str = "\u{05D3}";
    const P: &str = "\u{05E4}";

    fn word(parts: &[&str]) -> String {
        parts.concat()
    }

    #[test]
    fn content_range_drops_enumerators_only() {
        // Otzaria's content_range_drops_enumerators_only, letters substituted
        let w1 = word(&[A, B, G]);
        let w2 = word(&[D, A, B]);
        for (line, content) in [
            (format!("({G}) {w1} {w2}"), format!("{w1} {w2}")),
            (format!("{w1} {w2} {{{P}}}"), format!("{w1} {w2}")),
            (format!("[{A}{B}] {w1} ({A}):"), w1.clone()),
            (format!("({w1} {w2}) {w1}"), format!("({w1} {w2}) {w1}")),
            ("(abc) x".to_string(), "(abc) x".to_string()),
            ("() x".to_string(), "() x".to_string()),
            (format!("({A}{B}{G}{D}{A}{B}) x"), format!("({A}{B}{G}{D}{A}{B}) x")),
            (format!("({A})"), String::new()),
        ] {
            assert_eq!(&line[content_range(&line)], content, "{line:?}");
        }
    }

    #[test]
    fn content_range_keeps_paired_readings() {
        let (x, y) = (word(&[A, B]), word(&[G, D]));
        for (line, content) in [
            (format!("{D}{D} ({x}) [{y}]"), format!("{D}{D} ({x}) [{y}]")),
            (format!("({x}) [{y}] {D}{D}"), format!("({x}) [{y}] {D}{D}")),
            (format!("({A}) ({x}) [{y}] {{{P}}}"), format!("({x}) [{y}]")),
        ] {
            assert_eq!(&line[content_range(&line)], content, "{line:?}");
        }
    }

    #[test]
    fn edges_are_word_positions() {
        let w = word(&[A, B, G]);
        // (a) w w {p}: words 0..4, content 1..=2
        let e = line_edges(&format!("({A}) {w} {w} {{{P}}}"));
        assert_eq!(e, LineEdges { words: 4, first: Some(1), last: Some(2) });
        // trailing number (49) and a colon
        let e = line_edges(&format!("{w} {w} (49):"));
        assert_eq!(e, LineEdges { words: 3, first: Some(0), last: Some(1) });
        // markup around the enumerator, nikud inside it
        let e = line_edges(&format!("<b>({A}\u{05B8})</b> {w} <small>[{B}]</small>"));
        assert_eq!(e, LineEdges { words: 3, first: Some(1), last: Some(1) });
        // only an enumerator: no content
        let e = line_edges(&format!("({A})"));
        assert_eq!(e, LineEdges { words: 1, first: None, last: None });
        // a paired reading is content (both readings share one position)
        let e = line_edges(&format!("{w} ({A}{B}) [{G}{D}]"));
        assert_eq!(e, LineEdges { words: 2, first: Some(0), last: Some(1) });
        // no enumerator
        assert_eq!(line_edges(&w), LineEdges { words: 1, first: Some(0), last: Some(0) });
        assert_eq!(line_edges(""), LineEdges::default());
    }
}
