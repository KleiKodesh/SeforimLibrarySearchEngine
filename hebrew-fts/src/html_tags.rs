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

/// True for a `sup` or `sub` tag name (open or close; the scanners drop the `/`).
pub(crate) fn is_script_tag(name: &[char]) -> bool {
    let start = usize::from(matches!(name.first(), Some('/')));
    let name = &name[start.min(name.len())..];
    name.len() == 3
        && name[0].to_ascii_lowercase() == 's'
        && name[1].to_ascii_lowercase() == 'u'
        && matches!(name[2].to_ascii_lowercase(), 'p' | 'b')
}

fn is_letter_or_mark(c: char) -> bool {
    crate::chars::is_hebrew_letter(c) || crate::chars::is_attached_mark(c)
}

/// A Hebrew letter, or a presentation form that folds to one letter and its marks.
fn is_script_letter(c: char) -> bool {
    match crate::chars::fold_presentation_form(c) {
        Some(folded) => {
            let mut folded = folded.chars();
            folded.next().is_some_and(crate::chars::is_hebrew_letter)
                && folded.all(crate::chars::is_attached_mark)
        }
        None => crate::chars::is_hebrew_letter(c),
    }
}

/// Zero-width chars (bidi controls, CGJ, U+FB1E) that must not change how a
/// neighbouring `<sup>` / `<sub>` is classified.
fn is_script_invisible(c: char) -> bool {
    crate::chars::is_invisible(c) || crate::chars::is_grapheme_joiner(c) || c == '\u{FB1E}'
}

/// The tag name of a tag body (text between `<` and `>`): an optional `/`,
/// then an ASCII alphanumeric run of 1..=10 chars.
fn script_tag_name(body: &str) -> Option<(bool, &str)> {
    let (closing, name) = match body.strip_prefix('/') {
        Some(name) => (true, name),
        None => (false, body),
    };
    let len = name.bytes().take_while(u8::is_ascii_alphanumeric).count();
    (1..=10).contains(&len).then(|| (closing, &name[..len]))
}

/// An inline (transparent) tag body: neither a word-break tag nor sup/sub.
fn is_inline_tag(body: &str) -> bool {
    match script_tag_name(body) {
        Some((_, name)) => {
            let name: Vec<char> = name.chars().collect();
            !is_word_break_tag(&name)
        }
        None => true,
    }
}

fn trim_formatting_start(mut s: &str) -> &str {
    loop {
        s = s.trim_start_matches(is_script_invisible);
        let Some(body) = s.strip_prefix('<') else {
            break;
        };
        let Some(gt) = body.find('>') else { break };
        if !is_inline_tag(&body[..gt]) {
            break;
        }
        s = &body[gt + 1..];
    }
    s
}

fn trim_formatting_end(mut s: &str) -> &str {
    loop {
        s = s.trim_end_matches(is_script_invisible);
        let Some(body) = s.strip_suffix('>') else {
            break;
        };
        let Some(lt) = body.rfind('<') else { break };
        if body[lt + 1..].contains('>') || !is_inline_tag(&body[lt + 1..]) {
            break;
        }
        s = &body[..lt];
    }
    s
}

fn trim_context_end(mut s: &str) -> &str {
    loop {
        let t = trim_formatting_end(s).trim_end_matches(crate::chars::is_word_mark);
        if t.len() == s.len() {
            return s;
        }
        s = t;
    }
}

fn trim_context_start(mut s: &str) -> &str {
    loop {
        let t = trim_formatting_start(s).trim_start_matches(crate::chars::is_word_mark);
        if t.len() == s.len() {
            return s;
        }
        s = t;
    }
}

/// The word `footnote-marker` in a tag's attributes (case-insensitive, whole word).
fn marks_footnote(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    let word = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    lower.match_indices("footnote-marker").any(|(i, f)| {
        !word(lower.as_bytes()[..i].last()) && !word(lower.as_bytes().get(i + f.len()))
    })
}

/// An open `<sup>` / `<sub>` whose `<` is at byte `lt` and `>` at byte `gt`
/// of `text` is TRANSPARENT (the word continues through it, like `<b>`) when
/// it sits between letters and wraps a hanging letter (issue #37, Otzaria
/// engine 5babbce `is_word_internal_script`):
/// - its attributes do not hold the word `footnote-marker` (always splits);
/// - before `<` (skipping marks, bidi / CGJ chars and inline tags) is a Hebrew
///   letter or a presentation form of one;
/// - its content (inline tags and zero-width chars skipped) is either ONE such
///   letter with optional marks (no mark required), or Hebrew letters and
///   marks with at least one mark (issue #2 H58, a vocalized letter run);
/// - the content ends at the matching close tag (`</sup>` / `</sub>`), and
///   after it (same skipping) comes a Hebrew letter.
///
/// Every other `<sup>` / `<sub>` ends the word. Same rule in FtsLib's
/// `HtmlBlockTags.IsTransparentSup`.
pub(crate) fn is_transparent_sup(text: &str, lt: usize, gt: usize) -> bool {
    let body = &text[lt + 1..gt];
    let Some((false, name)) = script_tag_name(body) else {
        return false;
    };
    if marks_footnote(body) {
        return false;
    }
    if !trim_context_end(&text[..lt])
        .chars()
        .next_back()
        .is_some_and(is_script_letter)
    {
        return false;
    }
    let content = trim_formatting_start(&text[gt + 1..]);
    let mut chars = content.chars();
    if !chars.next().is_some_and(is_script_letter) {
        return false;
    }
    // one hanging letter (marks optional) ...
    let mut rest = trim_context_start(chars.as_str());
    if !rest.starts_with('<') {
        // ... or a vocalized run of letters and marks (at least one mark)
        let Some(end) = content.find('<') else {
            return false;
        };
        let run = &content[..end];
        if !run
            .chars()
            .all(|c| is_letter_or_mark(c) || is_script_invisible(c))
            || !run.chars().any(crate::chars::is_attached_mark)
        {
            return false;
        }
        rest = trim_context_start(&content[end..]);
    }
    let Some(close) = rest.strip_prefix('<') else {
        return false;
    };
    let Some(close_gt) = close.find('>') else {
        return false;
    };
    matches!(script_tag_name(&close[..close_gt]), Some((true, c)) if c.eq_ignore_ascii_case(name))
        && trim_context_start(&close[close_gt + 1..])
            .chars()
            .next()
            .is_some_and(is_script_letter)
}

/// Whether the tag `text[lt..=gt]` (`<` at byte `lt`, its first `>` at
/// `gt`) ends a word, decided as the scanner decides it at the `>`: the name
/// runs to the first space or tab, `/` dropped, at most `TAG_NAME_CAP`
/// chars; a `<sup>` breaks unless [`is_transparent_sup`], and its `</sup>`
/// follows the open tag (`sup_transparent` carries that between the two).
/// The line view uses it so that a line pattern sees a word boundary exactly
/// where the index does.
pub(crate) fn tag_breaks_word(text: &str, lt: usize, gt: usize, sup_transparent: &mut bool) -> bool {
    let mut name = ['\0'; crate::scanner::TAG_NAME_CAP];
    let mut len = 0usize;
    for c in text[lt + 1..gt].chars() {
        if c == ' ' || c == '\t' {
            break;
        }
        if len < name.len() && c != '/' {
            name[len] = c;
            len += 1;
        }
    }
    let name = &name[..len];
    if is_script_tag(name) {
        if text.as_bytes()[lt + 1] == b'/' {
            !std::mem::take(sup_transparent)
        } else {
            *sup_transparent = is_transparent_sup(text, lt, gt);
            !*sup_transparent
        }
    } else {
        is_word_break_tag(name)
    }
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
