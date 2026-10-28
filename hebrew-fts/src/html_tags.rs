//! Port of FtsLib `HtmlBlockTags.cs` (KitveiHakodesh, git HEAD).
//!
//! Block-tag detection and HTML entity handling, shared by the scanner.

/// Returns true if the tag name is a block-level HTML element (acts as a word
/// separator). A leading `/` (closing tag) or `!` (comment/doctype) is skipped.
pub(crate) fn is_block_tag(name: &[char]) -> bool {
    let start = usize::from(matches!(name.first(), Some('/') | Some('!')));
    let name = &name[start.min(name.len())..];
    if name.is_empty() || name.len() > 10 {
        // longest block tag ("figcaption"/"blockquote") is 10 chars
        return false;
    }
    // ASCII-lowercase fold; a non-ASCII char can never match an ASCII tag name.
    let mut buf = [0u8; 10];
    for (i, &c) in name.iter().enumerate() {
        if !c.is_ascii() {
            return false;
        }
        buf[i] = (c as u8).to_ascii_lowercase();
    }
    matches!(
        &buf[..name.len()],
        b"p" | b"br"
            | b"hr"
            | b"li"
            | b"ul"
            | b"ol"
            | b"tr"
            | b"td"
            | b"th"
            | b"dd"
            | b"dt"
            | b"h1"
            | b"h2"
            | b"h3"
            | b"h4"
            | b"h5"
            | b"h6"
            | b"div"
            | b"pre"
            | b"nav"
            | b"main"
            | b"table"
            | b"aside"
            | b"header"
            | b"footer"
            | b"figure"
            | b"section"
            | b"article"
            | b"caption"
            | b"figcaption"
            | b"blockquote"
    )
}

/// Tags that END the current word: every block-level element, plus `sup`/`sub` —
/// their content (footnote and reference markers) is a separate logical token,
/// not part of the word they interrupt (the scanners make a `<sup>` that
/// [`is_transparent_sup`] accepts transparent instead). All other tags are TRANSPARENT: inline
/// formatting (`<b>`, `<i>`, `<small>`, `<span>`, …) interrupts words mid-letter
/// (emphasised letters, e.g. ורא<b>ה</b>), and breaking the word there would
/// index unfindable fragments.
pub(crate) fn is_word_break_tag(name: &[char]) -> bool {
    if is_block_tag(name) {
        return true;
    }
    let start = usize::from(matches!(name.first(), Some('/') | Some('!')));
    let name = &name[start.min(name.len())..];
    if name.len() != 3 {
        return false;
    }
    let c0 = name[0].to_ascii_lowercase();
    let c1 = name[1].to_ascii_lowercase();
    let c2 = name[2].to_ascii_lowercase();
    c0 == 's' && c1 == 'u' && (c2 == 'p' || c2 == 'b') // sup, sub
}

/// True for a `sup` tag name (open or close; the scanners drop the `/`).
pub(crate) fn is_sup(name: &[char]) -> bool {
    let start = usize::from(matches!(name.first(), Some('/')));
    let name = &name[start.min(name.len())..];
    name.len() == 3 && name.iter().map(|c| c.to_ascii_lowercase()).eq(['s', 'u', 'p'])
}

fn is_letter_or_mark(c: char) -> bool {
    crate::chars::is_hebrew_letter(c) || crate::chars::is_attached_mark(c)
}

/// A `<sup>` whose `<` is at byte `lt` and `>` at byte `gt` of `text` is
/// TRANSPARENT (the word continues through it, like `<b>`) only when its
/// content up to the next `<` is non-empty, holds only Hebrew letters and
/// attached marks with at least one mark, that `<` starts `</sup>`, and the
/// chars right before `<sup` and right after `</sup>` are Hebrew letters or
/// marks (a vocalized letter set mid-word, e.g. in Tanach). Every other
/// `<sup>` (a footnote or letter marker) ends the word. Same rule in FtsLib's
/// `HtmlBlockTags.IsTransparentSup`.
pub(crate) fn is_transparent_sup(text: &str, lt: usize, gt: usize) -> bool {
    if !text[..lt].chars().next_back().is_some_and(is_letter_or_mark) {
        return false;
    }
    let rest = &text[gt + 1..];
    let Some(close) = rest.find('<') else { return false };
    let inner = &rest[..close];
    if inner.is_empty() || !inner.chars().all(is_letter_or_mark) || !inner.chars().any(crate::chars::is_attached_mark) {
        return false;
    }
    let after = &rest[close..];
    after.len() >= 6
        && after.as_bytes()[..6].eq_ignore_ascii_case(b"</sup>")
        && after[6..].chars().next().is_some_and(is_letter_or_mark)
}

/// Advances `i` past a well-formed entity's closing `;` (entity = `&` at
/// `chars[*i]`, then a ≤10-unit name/number, then `;`). Leaves `i` unchanged
/// when malformed, so the caller consumes just the `&`.
///
/// No whitespace-vs-other classification: the scanner treats EVERY entity as a
/// word separator (the corpus has only whitespace and punctuation entities).
///
/// `chars` is the scanner's `(byte, utf16, char)` table; the ≤10 limit counts
/// UTF-16 units, matching the C# original.
#[cfg(test)] // only the reference scanner (scanner_reference.rs) uses the table form
pub(crate) fn skip_entity(chars: &[(usize, usize, char)], len: usize, i: &mut usize) {
    let start = *i + 1;
    let mut end = start;
    if start >= len {
        return;
    }
    let start_u16 = chars[start].1;
    while end < len && chars[end].1 - start_u16 < 10 && chars[end].2 != ';' {
        end += 1;
    }
    if end < len && chars[end].2 == ';' && end > start {
        *i = end; // caller's loop increment moves past the ';'
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cv(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    #[test]
    fn block_tags() {
        for t in ["p", "br", "div", "h3", "table", "blockquote", "figcaption"] {
            assert!(is_block_tag(&cv(t)), "{t}");
            assert!(is_block_tag(&cv(&format!("/{t}"))), "/{t}");
        }
        for t in ["b", "i", "span", "small", "a", "h7", "h0", ""] {
            assert!(!is_block_tag(&cv(t)), "{t}");
        }
        assert!(is_block_tag(&cv("DIV")));
        assert!(is_block_tag(&cv("!DOCTYPE")) == false);
    }

    #[test]
    fn word_break_tags() {
        assert!(is_word_break_tag(&cv("sup")));
        assert!(is_word_break_tag(&cv("/sub")));
        assert!(is_word_break_tag(&cv("SUP")));
        assert!(is_word_break_tag(&cv("p")));
        assert!(!is_word_break_tag(&cv("b")));
        assert!(!is_word_break_tag(&cv("span")));
    }

    fn table(s: &str) -> Vec<(usize, usize, char)> {
        let mut u = 0;
        s.char_indices()
            .map(|(b, c)| {
                let e = (b, u, c);
                u += c.len_utf16();
                e
            })
            .collect()
    }

    #[test]
    fn entity_skip() {
        // well-formed: lands on ';'
        let t = table("&nbsp;x");
        let mut i = 0;
        skip_entity(&t, t.len(), &mut i);
        assert_eq!(i, 5);
        // bare '&': unchanged
        let t = table("& x");
        let mut i = 0;
        skip_entity(&t, t.len(), &mut i);
        assert_eq!(i, 0);
        // '&;' (empty name): unchanged
        let t = table("&;x");
        let mut i = 0;
        skip_entity(&t, t.len(), &mut i);
        assert_eq!(i, 0);
        // too long before ';': unchanged
        let t = table("&abcdefghijk;");
        let mut i = 0;
        skip_entity(&t, t.len(), &mut i);
        assert_eq!(i, 0);
    }
}
