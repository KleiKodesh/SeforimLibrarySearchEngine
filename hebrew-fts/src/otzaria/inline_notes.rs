// Ported verbatim from otzaria_search_engine 7.2.0 (MIT, Copyright (c) 2026 otzaria),
// rust/src/inline_notes.rs; see THIRD-PARTY-NOTICES.md. Changes: the test modules dropped.
//! Port of the app's `rehydrateInlineNotes` (`inline_notes_utils.dart`): from schema 7 a
//! footnote's body is in `line_note`, and only its `<sup>` marker stays in the line. The
//! indexing path puts each body back, so that words found only in footnotes are searchable.
//!
//! Offsets follow the app's UTF-16 strings: `charPos` counts UTF-16 units, an entity counts
//! as one. The patterns are Dart (JavaScript) `RegExp`s: ASCII case folding, ASCII `\b` and
//! JavaScript's `\s`.

use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::HashMap;

/// One `line_note` row of a line, in `noteIndex` order. `char_pos` is -1 when it has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookNote {
    /// UTF-16 offset in the stored line, counting each HTML entity as one unit; -1 if absent.
    pub char_pos: i64,
    /// `line_note.hasMarker`: the note had a marker sup (an empty one still counts).
    pub has_marker: bool,
    /// Original footnote body HTML, without the surrounding footnote element.
    pub body: String,
}

/// A book's notes by `lineIndex`.
pub(crate) type BookNotes = HashMap<i64, Vec<BookNote>>;

const JS_SPACE: &str = r"[\t\n\x0B\x0C\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]";
const B: &str = r"(?-u:\b)";

static FOOTNOTE_BODY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!(
        r#"(?s)<(?i-u:i){B}[^>]*{B}(?i-u:class){JS_SPACE}*={JS_SPACE}*"[^"]*{B}(?i-u:footnote){B}[^"]*"[^>]*>(.*?)</(?i-u:i)>"#
    ))
    .unwrap()
});

// The generator leaves footnote tags inside any sup in the marker's HTML.
static SUP: Lazy<Regex> =
    Lazy::new(|| Regex::new(&format!(r"(?s)<(?i-u:sup){B}[^>]*>.*?</(?i-u:sup)>")).unwrap());

/// Whether a body outside a sup is already present. Walk both sets of ranges in
/// order, so checking many markers and notes stays linear in the line's length.
fn has_rehydrated_body(html: &str, sup_ranges: &[std::ops::Range<usize>]) -> bool {
    let mut bodies = FOOTNOTE_BODY.find_iter(html).peekable();
    if bodies.peek().is_none() {
        return false;
    }
    let mut sups = sup_ranges.iter().peekable();
    bodies.any(|body| {
        while sups.peek().is_some_and(|sup| sup.end <= body.start()) {
            sups.next();
        }
        !sups.peek().is_some_and(|sup| sup.start <= body.start())
    })
}

// Shared with the app's inline_notes_utils.dart. A closing block or unknown tag
// stays after the restored body; crossing it can merge the next paragraph's words.
const INLINE_TAG_NAMES: &[&str] = &[
    "b", "i", "em", "strong", "u", "s", "strike", "span", "sup", "sub", "small", "big", "font",
    "del", "ins", "a", "abbr", "acronym", "cite", "code", "dfn", "kbd", "q", "samp", "tt", "var",
    "mark", "bdi", "bdo", "ruby", "rt", "rp",
];

static FOOTNOTE_MARKER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!(
        r"(?s)<(?i-u:sup){B}[^>]*{B}(?i-u:footnote-marker){B}[^>]*>.*?</(?i-u:sup)>"
    ))
    .unwrap()
});

static FOOTNOTE_BODY_START: Lazy<Regex> = Lazy::new(|| {
    Regex::new(&format!(
        r#"^{JS_SPACE}*<(?i-u:i){B}[^>]*{B}(?i-u:class){JS_SPACE}*={JS_SPACE}*"[^"]*{B}(?i-u:footnote){B}"#
    ))
    .unwrap()
});

/// The byte ranges of markers whose body was moved out: those not followed by one.
fn stripped_markers(html: &str) -> Vec<std::ops::Range<usize>> {
    FOOTNOTE_MARKER
        .find_iter(html)
        .filter(|m| !FOOTNOTE_BODY_START.is_match(&html[m.end()..]))
        .map(|m| m.range())
        .collect()
}

/// Where a body goes: a byte offset, and whether the app's UTF-16 offset falls between the
/// two surrogates of the character starting there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Insertion {
    at: usize,
    splits_char: bool,
}

/// `_rawInsertionPoint`: after the `visible`-th visible UTF-16 unit, with the closing tags
/// right after it only when they close inline formatting (e.g. `</sup>`).
/// Stop before block boundaries so the body remains in its original paragraph.
fn raw_insertion_point(html: &str, visible: i64) -> Insertion {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let end_of_line = Insertion {
        at: len,
        splits_char: false,
    };
    let find = |from: usize, byte: u8| {
        bytes[from..]
            .iter()
            .position(|&b| b == byte)
            .map(|p| p + from)
    };
    let mut seen = 0i64;
    let mut i = 0usize;
    while i < len && seen < visible {
        match bytes[i] {
            b'<' => match find(i, b'>') {
                Some(close) => {
                    i = close + 1;
                    continue;
                }
                None => return end_of_line,
            },
            b'&' => {
                i = match find(i + 1, b';') {
                    Some(end) if html[i..end].encode_utf16().take(10).count() < 10 => end + 1,
                    _ => i + 1,
                };
            }
            _ => {
                let ch = html[i..].chars().next().expect("i is on a char boundary");
                if ch.len_utf16() == 2 {
                    if seen + 1 == visible {
                        return Insertion {
                            at: i,
                            splits_char: true,
                        };
                    }
                    seen += 1;
                }
                i += ch.len_utf8();
            }
        }
        seen += 1;
    }
    if seen < visible {
        return end_of_line;
    }
    Insertion {
        at: after_inline_closings(html, i),
        splits_char: false,
    }
}

fn after_inline_closings(html: &str, mut at: usize) -> usize {
    while html[at..].starts_with("</") {
        let Some(close) = html[at..].find('>').map(|close| at + close) else {
            break;
        };
        let name = html[at + 2..close].trim_end();
        if !INLINE_TAG_NAMES
            .iter()
            .any(|inline| name.eq_ignore_ascii_case(inline))
        {
            break;
        }
        at = close + 1;
    }
    at
}

fn only_tags_between(html: &str, mut at: usize, end: usize) -> bool {
    while at < end {
        if html.as_bytes()[at] != b'<' {
            return false;
        }
        let Some(close) = html[at..end].find('>') else {
            return false;
        };
        at += close + 1;
    }
    at == end
}

fn footnote_html(body: &str) -> String {
    format!("<i class=\"footnote\">{body}</i>")
}

/// Restores schema 7 footnote HTML for database readers and indexing.
///
/// Supply notes in database `noteIndex` order. Each body is inserted at its `charPos`;
/// a note without one is inserted after its marker
/// (the k-th stripped `<sup>` belongs to the k-th note with a marker), else at the end. A line
/// that already holds a body outside a sup is left alone. `None` when unchanged.
pub fn rehydrate_inline_notes(line: &str, notes: &[BookNote]) -> Option<String> {
    if notes.is_empty() {
        return None;
    }
    let sups: Vec<_> = SUP.find_iter(line).map(|sup| sup.range()).collect();
    if has_rehydrated_body(line, &sups) {
        return None;
    }
    let markers = stripped_markers(line);
    let mut anchored_seen = 0;
    let mut previous_position: Option<(i64, Insertion)> = None;
    let mut inserts: Vec<(Insertion, usize)> = Vec::with_capacity(notes.len());
    for (order, note) in notes.iter().enumerate() {
        let marker = if note.has_marker {
            markers.get(anchored_seen)
        } else {
            None
        };
        let mut at = if note.char_pos >= 0 {
            let at = raw_insertion_point(line, note.char_pos);
            // The generator never extracts a body inside a sup. Empty trailing
            // markup can leave the visible scan there, including for a second,
            // unanchored body after the same marker. Its start is excluded: an
            // unanchored body at that offset may precede the sup entirely.
            let inside = sups
                .get(sups.partition_point(|sup| sup.end <= at.at))
                .filter(|sup| sup.start < at.at);
            // Empty markers have no visible width. A charPos scan can therefore
            // stop before their opening formatting tags. Follow only this note's
            // corresponding marker, and only across complete tags with no text.
            let empty_marker = marker.filter(|marker| {
                at.at <= marker.start && only_tags_between(line, at.at, marker.start)
            });
            match inside.or(empty_marker) {
                Some(sup) => Insertion {
                    at: after_inline_closings(line, sup.end),
                    splits_char: false,
                },
                None => at,
            }
        } else if let Some(marker) = marker {
            Insertion {
                at: marker.end,
                splits_char: false,
            }
        } else {
            Insertion {
                at: line.len(),
                splits_char: false,
            }
        };
        if note.char_pos >= 0 {
            // Adjacent bodies share charPos. Keep their source order even when
            // only the first is anchored to an empty marker at this offset.
            if let Some((char_pos, previous)) = previous_position {
                if char_pos == note.char_pos && previous > at {
                    at = previous;
                }
            }
            previous_position = Some((note.char_pos, at));
        } else {
            previous_position = None;
        }
        if note.has_marker {
            anchored_seen += 1;
        }
        inserts.push((at, order));
    }
    inserts.sort();
    let mut out =
        String::with_capacity(line.len() + notes.iter().map(|n| n.body.len() + 30).sum::<usize>());
    let mut last = 0;
    for (n, &(insert, order)) in inserts.iter().enumerate() {
        out.push_str(&line[last..insert.at]);
        last = insert.at;
        // The app's string holds a lone surrogate on each side, which its UTF-8 encoder
        // writes as U+FFFD.
        let opens_split = insert.splits_char && (n == 0 || inserts[n - 1].0 != insert);
        if opens_split {
            out.push('\u{FFFD}');
        }
        out.push_str(&footnote_html(&notes[order].body));
        let closes_split = insert.splits_char && inserts.get(n + 1).map(|i| i.0) != Some(insert);
        if closes_split {
            out.push('\u{FFFD}');
            last += line[insert.at..].chars().next().map_or(0, char::len_utf8);
        }
    }
    out.push_str(&line[last..]);
    Some(out)
}

/// `rehydrateInlineNotesInText` on one stored row: its `\n`-separated lines are lines
/// `first_line..` of the book's text, as the app splits the joined text.
pub(crate) fn rehydrate_row(row: &str, first_line: i64, notes: &BookNotes) -> Option<String> {
    if !row.contains('\n') {
        return rehydrate_inline_notes(row, notes.get(&first_line)?);
    }
    let mut changed = false;
    let lines: Vec<std::borrow::Cow<str>> = row
        .split('\n')
        .enumerate()
        .map(|(n, line)| {
            match notes
                .get(&(first_line + n as i64))
                .and_then(|notes| rehydrate_inline_notes(line, notes))
            {
                Some(rehydrated) => {
                    changed = true;
                    rehydrated.into()
                }
                None => line.into(),
            }
        })
        .collect();
    changed.then(|| lines.join("\n"))
}

