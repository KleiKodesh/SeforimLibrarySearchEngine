//! Otzaria's semantic TEXT CHECK, ported step for step (issue #18).
//!
//! A vector of an Otzaria `.oxv` is stored under the chunk key of the text it
//! was embedded from (the first 16 bytes of the SHA-256 of that text). Before
//! a hit is shown, Otzaria recomputes the key of the line from the library and
//! drops the line when it no longer matches (all 128 bits in segment format
//! 2, the first 64 in format 3). This module computes the same keys from a
//! seforim.db book's raw rows, which the HOST reads (no database code here):
//!
//! 1. [`book_lines`]: the book's text as Otzaria's indexing path builds it
//!    (rows joined with `\n`, footnote bodies put back, `data:` URIs removed,
//!    split on `\n`, so a row holding `\n` is several lines), each line's
//!    normalized text ([`normalize_text_for_indexing`]) and section (a line
//!    starting `<h` opens one), and the row / sub-line of each ordinal.
//! 2. [`Chunker`]: Otzaria's production chunking (Meivin round 2, windows):
//!    a line's embedded text, with neighbour context for a short line, and
//!    the extra windows of a long one, and their [`ChunkKey`]s.
//! 3. [`verify`] / [`locate`]: does a line hold a stored key.
//!
//! Sources (pinned): otzaria-semantic-search d2a782a `src/semantic/`
//! (`chunker.rs`, `chunk_key.rs`, `recipe.rs`) and otzaria_search_engine
//! (`semantic_keys.rs`, `semantic_resolver.rs`, `line_source.rs`,
//! `inline_notes.rs`, `hebrew_query.rs`, `api/search_engine.rs`). Each item
//! cites the function it ports.

use regex::Regex;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Chunk key (otzaria-semantic-search chunk_key.rs)
// ---------------------------------------------------------------------------

/// `chunk_key.rs` `ChunkKey`: the first 16 bytes of the SHA-256 of an embedded text.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ChunkKey(pub [u8; 16]);

impl ChunkKey {
    /// `ChunkKey::of`.
    pub fn of(embedded_text: &str) -> Self {
        let digest = Sha256::digest(embedded_text.as_bytes());
        let mut key = [0u8; 16];
        key.copy_from_slice(&digest[..16]);
        Self(key)
    }

    /// A key as stored: 16 bytes (format 2) or 8 (format 3, read narrowed).
    pub fn from_stored(bytes: &[u8]) -> Option<Self> {
        let mut key = [0u8; 16];
        match bytes.len() {
            16 | 8 => key[..bytes.len()].copy_from_slice(bytes),
            _ => return None,
        }
        Some(Self(key))
    }

    /// `ChunkKey::narrowed`: the first eight bytes, the rest zero.
    pub fn narrowed(self) -> Self {
        let mut key = [0u8; 16];
        key[..8].copy_from_slice(&self.0[..8]);
        Self(key)
    }

    /// `ChunkKey::is_narrow`.
    pub fn is_narrow(self) -> bool {
        self.0[8..] == [0; 8]
    }

    /// `ChunkKey::same_text`: equal, or equal in the first eight bytes when either is narrow.
    pub fn same_text(self, other: Self) -> bool {
        self == other || ((self.is_narrow() || other.is_narrow()) && self.0[..8] == other.0[..8])
    }

    /// `ChunkKey::column_value`: the first eight bytes, big-endian.
    pub fn column_value(self) -> u64 {
        u64::from_be_bytes(self.0[..8].try_into().unwrap())
    }
}

// ---------------------------------------------------------------------------
// Chunking (otzaria-semantic-search chunker.rs, recipe.rs; semantic_keys.rs)
// ---------------------------------------------------------------------------

/// recipe.rs `PASSAGE_PREFIX` (embedding text version 2).
pub const PASSAGE_PREFIX: &str = "[PASSAGE] ";

/// chunker.rs `ChunkerConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkerConfig {
    pub min_meaningful_chars: usize,
    pub context_window_lines: usize,
    pub max_chunk_chars: usize,
    pub min_embeddable_chars: usize,
    pub chunking_version: u32,
    pub embedding_text_version: u32,
    pub normalization_version: u32,
    pub window_chars: usize,
    pub window_overlap_chars: usize,
    pub max_windows_per_line: usize,
}

impl ChunkerConfig {
    /// semantic_keys.rs `production_chunking` (Meivin round 2 with windows).
    pub fn production() -> Self {
        Self {
            min_meaningful_chars: 20,
            context_window_lines: 2,
            max_chunk_chars: 512,
            min_embeddable_chars: 5,
            chunking_version: 2,
            embedding_text_version: 2,
            normalization_version: 1,
            window_chars: 512,
            window_overlap_chars: 128,
            max_windows_per_line: 6,
        }
    }

    /// chunker.rs `ChunkerConfig::anchor`: the same recipe without windows.
    pub fn anchor(&self) -> Self {
        Self {
            chunking_version: 1,
            window_chars: 0,
            window_overlap_chars: 0,
            max_windows_per_line: 0,
            ..self.clone()
        }
    }

    /// chunker.rs `ChunkerConfig::identity` (model.json `chunking_identity`).
    pub fn identity(&self) -> u64 {
        let mut hasher = Sha256::new();
        for field in [
            self.min_meaningful_chars as u64,
            self.context_window_lines as u64,
            self.max_chunk_chars as u64,
            self.min_embeddable_chars as u64,
            u64::from(self.chunking_version),
            u64::from(self.embedding_text_version),
            u64::from(self.normalization_version),
        ] {
            hasher.update(field.to_le_bytes());
        }
        if self.chunking_version >= 2 {
            for field in [
                self.window_chars as u64,
                self.window_overlap_chars as u64,
                self.max_windows_per_line as u64,
            ] {
                hasher.update(field.to_le_bytes());
            }
        }
        let digest = hasher.finalize();
        u64::from_le_bytes(digest[..8].try_into().unwrap())
    }

    /// semantic_keys.rs `pardes_chunking`: the production geometry with embedding
    /// text 3 (no passage prefix) and normalization 2 (pardes-clean-1).
    pub fn pardes() -> Self {
        Self { embedding_text_version: 3, normalization_version: 2, ..Self::production() }
    }

    /// semantic_keys.rs `keyed_chunkings`: production, its anchor, pardes.
    pub fn keyed() -> [Self; 3] {
        let production = Self::production();
        let anchor = production.anchor();
        [production, anchor, Self::pardes()]
    }

    /// semantic_keys.rs `keyed_chunking`: the one of [`Self::keyed`] whose identity is
    /// `identity`.
    pub fn by_identity(identity: u64) -> Option<Self> {
        Self::keyed().into_iter().find(|c| c.identity() == identity)
    }
}

/// One line as the recipe sees it (chunk_key.rs `LineRef`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRef<'a> {
    pub text: &'a str,
    pub section: u64,
}

/// chunker.rs `EmbeddedWindow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedWindow {
    pub ordinal: u8,
    pub chars: std::ops::Range<u32>,
    pub text: String,
}

/// chunker.rs `Chunker`.
#[derive(Debug, Clone)]
pub struct Chunker {
    config: ChunkerConfig,
}

/// Why a configuration is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedRecipe(pub String);

impl Chunker {
    /// chunker.rs `Chunker::new` with `check_windows`: recipe.rs embedding text
    /// versions 1, 2, 3 and normalization versions 1, 2.
    pub fn new(config: ChunkerConfig) -> Result<Self, UnsupportedRecipe> {
        if !matches!(config.embedding_text_version, 1..=3) || !matches!(config.normalization_version, 1 | 2) {
            return Err(UnsupportedRecipe(format!(
                "embedding text {} / normalization {} not implemented",
                config.embedding_text_version, config.normalization_version
            )));
        }
        let windows = [config.window_chars, config.window_overlap_chars, config.max_windows_per_line];
        match config.chunking_version {
            1 if windows.iter().all(|&w| w == 0) => {}
            2 if windows.iter().all(|&w| w > 0)
                && config.window_overlap_chars * 2 < config.window_chars
                && config.max_windows_per_line <= 255 => {}
            v => return Err(UnsupportedRecipe(format!("chunking version {v} / windows {windows:?}"))),
        }
        Ok(Self { config })
    }

    pub fn production() -> Self {
        Self::new(ChunkerConfig::production()).unwrap()
    }

    pub fn config(&self) -> &ChunkerConfig {
        &self.config
    }

    /// chunker.rs `Chunker::embedded_text` (= `anchored_text`): window 0's text.
    pub fn embedded_text(&self, lines: &[LineRef<'_>], index: usize) -> Option<String> {
        let text = lines[index].text;
        let char_count = text.trim().chars().count();
        if char_count < self.config.min_embeddable_chars {
            return None;
        }
        let context;
        let embedding_text = if char_count < self.config.min_meaningful_chars {
            context = self.build_context_text(lines, index);
            context.as_str()
        } else {
            text
        };
        let truncated = truncate_to_chars(embedding_text.trim(), self.config.max_chunk_chars);
        let normalized = self.normalize(truncated);
        if normalized.trim().is_empty() {
            return None;
        }
        Some(self.passage_text(&normalized))
    }

    /// chunker.rs `Chunker::windows_of`: window 0, then (version 2) the windows of a
    /// long line standing alone, a repeated key recorded once.
    pub fn embedded_windows(&self, lines: &[LineRef<'_>], index: usize) -> Vec<EmbeddedWindow> {
        let Some(first) = self.embedded_text(lines, index) else {
            return Vec::new();
        };
        let text = lines[index].text.trim();
        let n = text.chars().count();
        let mut out = vec![EmbeddedWindow {
            ordinal: 0,
            chars: 0..n.min(self.config.max_chunk_chars) as u32,
            text: first,
        }];
        if self.config.chunking_version != 2
            || n < self.config.min_meaningful_chars
            || n <= self.config.max_chunk_chars
        {
            return out;
        }
        let chars: Vec<char> = text.chars().collect();
        let mut keys = vec![ChunkKey::of(&out[0].text)];
        let spans = window_spans(
            &chars,
            self.config.max_chunk_chars,
            self.config.window_chars,
            self.config.window_overlap_chars,
            self.config.max_windows_per_line,
        );
        for (ordinal, (start, end)) in spans.into_iter().enumerate().skip(1) {
            let piece: String = chars[start..end].iter().collect();
            let normalized = self.normalize(&piece);
            if normalized.trim().is_empty() {
                continue;
            }
            let text = self.passage_text(&normalized);
            let key = ChunkKey::of(&text);
            if keys.contains(&key) {
                continue;
            }
            keys.push(key);
            out.push(EmbeddedWindow { ordinal: ordinal as u8, chars: start as u32..end as u32, text });
        }
        out
    }

    /// The key of every window of a line: `(window, key)`.
    pub fn window_keys(&self, lines: &[LineRef<'_>], index: usize) -> Vec<(u8, ChunkKey)> {
        self.embedded_windows(lines, index)
            .into_iter()
            .map(|w| (w.ordinal, ChunkKey::of(&w.text)))
            .collect()
    }

    /// The text a semantic hit's passage highlight is computed on (otzaria_search_engine
    /// 7.2.0 `embedded_window_texts`, spaces collapsed): for a short line (not an alias
    /// record) the capped context it was embedded with, for window `window` past the first
    /// of a long line an ellipsis + its piece + an ellipsis (none after the line's end);
    /// `None` when the line's own text is shown. `lines` / `index` as for [`Self::embedded_text`].
    pub fn display_window_text(&self, lines: &[LineRef<'_>], index: usize, window: u8) -> Option<String> {
        let texts: Vec<&str> = lines.iter().map(|l| l.text).collect();
        self.display_window_text_with(&texts, |at| Some(lines[at].section), index, window)
    }

    /// [`Self::display_window_text`] with each line's section read through `section`, `None`
    /// when unknown: as Otzaria's `context_window`, a short line whose window needs a section
    /// that is unknown has no window (its own text is shown).
    pub fn display_window_text_with(
        &self,
        texts: &[&str],
        section: impl Fn(usize) -> Option<u64>,
        index: usize,
        window: u8,
    ) -> Option<String> {
        let text = texts[index];
        let length = text.trim().chars().count();
        if window != ALIAS_WINDOW && (self.config.min_embeddable_chars..self.config.min_meaningful_chars).contains(&length) {
            // short_line_windows: `context_window` (production reach), none of one line.
            if let Some(range) = context_window(texts.len(), index, &section) {
                if range.start() != range.end() {
                    let joined = texts[range].join(" ");
                    return Some(collapse_whitespace(truncate_to_chars(joined.trim(), self.config.max_chunk_chars)));
                }
            }
        }
        // long_line_window_text / semantic_keys.rs `window_span_with`.
        if window == 0 || length <= ChunkerConfig::production().max_chunk_chars {
            return None;
        }
        let alone = [LineRef { text, section: 0 }];
        let found = self.embedded_windows(&alone, 0).into_iter().find(|w| w.ordinal == window)?;
        let trimmed = text.trim();
        let at = |n: u32| trimmed.char_indices().nth(n as usize).map_or(trimmed.len(), |(at, _)| at);
        let piece = collapse_whitespace(&trimmed[at(found.chars.start)..at(found.chars.end)]);
        Some(if (found.chars.end as usize) < length {
            format!("\u{2026}{piece}\u{2026}")
        } else {
            format!("\u{2026}{piece}")
        })
    }

    /// chunker.rs `build_context_text`: the line and up to `context_window_lines`
    /// neighbours on each side within its section, joined with spaces.
    fn build_context_text(&self, lines: &[LineRef<'_>], index: usize) -> String {
        let section = lines[index].section;
        let mut start = index;
        for _ in 0..self.config.context_window_lines {
            if start == 0 || lines[start - 1].section != section {
                break;
            }
            start -= 1;
        }
        let mut end = index;
        for _ in 0..self.config.context_window_lines {
            if end + 1 >= lines.len() || lines[end + 1].section != section {
                break;
            }
            end += 1;
        }
        lines[start..=end].iter().map(|l| l.text).collect::<Vec<_>>().join(" ")
    }
}

impl Chunker {
    /// recipe.rs `TextNormalizationRecipe::apply`: 1 = as supplied, 2 = pardes-clean-1.
    fn normalize<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        match self.config.normalization_version {
            2 => std::borrow::Cow::Owned(pardes_clean(text)),
            _ => std::borrow::Cow::Borrowed(text),
        }
    }

    /// recipe.rs `EmbeddingTextRecipe::passage_text`: versions 1 and 3 the text as
    /// is, version 2 the role prefix before the trimmed text.
    fn passage_text(&self, text: &str) -> String {
        match self.config.embedding_text_version {
            2 => format!("{PASSAGE_PREFIX}{}", text.trim()),
            _ => text.to_string(),
        }
    }
}

/// pardes_clean.rs `space`.
fn pardes_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{000b}'
            | '\u{000c}'
            | '\r'
            | ' '
            | '\u{0085}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

/// pardes_clean.rs `entity`.
fn pardes_entity(s: &str) -> Option<char> {
    match s {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let (digits, radix, max) = if let Some(t) = s.strip_prefix("#x").or_else(|| s.strip_prefix("#X")) {
                (t, 16, 6)
            } else {
                (s.strip_prefix('#')?, 10, 7)
            };
            if digits.is_empty()
                || digits.len() > max
                || !digits.chars().all(|c| c.is_ascii_hexdigit() && (radix == 16 || c.is_ascii_digit()))
            {
                return None;
            }
            let n = u32::from_str_radix(digits, radix).ok()?;
            if n == 0 {
                None
            } else {
                char::from_u32(n)
            }
        }
    }
}

/// pardes_clean.rs `clean` (pardes-clean-1): tags to a space, one entity pass,
/// whitespace runs to one space (none at either end), gershayim / geresh to ASCII.
pub fn pardes_clean(text: &str) -> String {
    let mut tags = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find('<') {
        tags.push_str(&rest[..i]);
        if let Some(j) = rest[i..].find('>') {
            tags.push(' ');
            rest = &rest[i + j + 1..];
        } else {
            tags.push_str(&rest[i..]);
            rest = "";
            break;
        }
    }
    tags.push_str(rest);
    let mut decoded = String::with_capacity(tags.len());
    let mut rest = tags.as_str();
    while let Some(i) = rest.find('&') {
        decoded.push_str(&rest[..i]);
        let tail = &rest[i + 1..];
        // Every recognized entity has at most eight ASCII characters before ';'.
        let candidate = tail.find(';').filter(|&j| j <= 8);
        if let Some((j, c)) = candidate.and_then(|j| pardes_entity(&tail[..j]).map(|c| (j, c))) {
            decoded.push(c);
            rest = &tail[j + 1..];
        } else {
            decoded.push('&');
            rest = tail;
        }
    }
    decoded.push_str(rest);
    let mut out = String::with_capacity(decoded.len());
    let mut pending_space = false;
    for c in decoded.chars() {
        if pardes_space(c) {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(match c {
            '\u{05f4}' => '"',
            '\u{05f3}' => '\'',
            _ => c,
        });
    }
    out
}

/// semantic_keys.rs `context_window`: the lines a short line at `position` is embedded
/// with, up to the production recipe's `context_window_lines` on each side within its
/// section; `None` when a section it reads is unknown.
fn context_window(len: usize, position: usize, section: impl Fn(usize) -> Option<u64>) -> Option<std::ops::RangeInclusive<usize>> {
    let reach = ChunkerConfig::production().context_window_lines;
    let own = section(position)?;
    let mut start = position;
    for _ in 0..reach {
        if start == 0 || section(start - 1)? != own {
            break;
        }
        start -= 1;
    }
    let mut end = position;
    for _ in 0..reach {
        if end + 1 >= len || section(end + 1)? != own {
            break;
        }
        end += 1;
    }
    Some(start..=end)
}

/// note_spans.rs `DisplayText::collapse_whitespace` on plain text: whitespace runs to one
/// space, none leading or trailing.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending = false;
    for c in s.chars() {
        if c.is_whitespace() {
            pending |= !out.is_empty();
            continue;
        }
        if pending {
            out.push(' ');
            pending = false;
        }
        out.push(c);
    }
    out
}

/// chunker.rs `truncate_to_chars`.
fn truncate_to_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((at, _)) => &s[..at],
        None => s,
    }
}

/// chunker.rs `window_spans`.
fn window_spans(chars: &[char], first: usize, length: usize, overlap: usize, max_windows: usize) -> Vec<(usize, usize)> {
    let n = chars.len();
    let mut out = vec![(0, n.min(first))];
    while let Some(&(previous_start, previous_end)) = out.last() {
        if previous_end >= n || out.len() >= max_windows {
            break;
        }
        let mut start = previous_end.saturating_sub(overlap).max(previous_start + 1);
        if chars[start - 1] != ' ' {
            if let Some(space) = (start..previous_end).find(|&at| chars[at] == ' ') {
                start = space + 1;
            }
        }
        let mut end = n.min(start + length);
        if end < n {
            let from = start + length / 2;
            if from <= end {
                if let Some(space) = (from..=end).rev().find(|&at| chars[at] == ' ') {
                    end = space;
                }
            }
        }
        out.push((start, end));
    }
    out
}

// ---------------------------------------------------------------------------
// Verification (semantic_resolver.rs)
// ---------------------------------------------------------------------------

/// semantic_resolver.rs `RECOMPUTE_REACH`: lines on each side of a hint searched for a key.
pub const RECOMPUTE_REACH: usize = 16;

/// semantic_resolver.rs `ALIAS_WINDOW`: a short line found through a neighbour's vector.
pub const ALIAS_WINDOW: u8 = 255;

/// One stored record to check: the line ordinal, its window and its key as stored
/// (16 bytes in format 2, 8 in format 3). For an alias record ([`ALIAS_WINDOW`]) pass
/// the alias's anchor (the short line's own key) with window 0, as `alias_line` does.
#[derive(Debug, Clone)]
pub struct StoredKey<'a> {
    pub ordinal: usize,
    pub window: u8,
    pub key: &'a [u8],
}

/// Whether line `ordinal` holds `key` as `window`, compared at the stored width
/// (`ChunkKey::same_text`): the check semantic_resolver.rs makes before a line is shown.
pub fn holds(chunker: &Chunker, lines: &[LineRef<'_>], ordinal: usize, window: u8, key: ChunkKey) -> bool {
    if ordinal >= lines.len() {
        return false;
    }
    if window == 0 {
        return chunker.embedded_text(lines, ordinal).is_some_and(|t| ChunkKey::of(&t).same_text(key));
    }
    chunker.window_keys(lines, ordinal).iter().any(|&(w, k)| w == window && k.same_text(key))
}

/// [`holds`] for each record, in order.
pub fn verify(chunker: &Chunker, lines: &[LineRef<'_>], records: &[StoredKey<'_>]) -> Vec<bool> {
    records
        .iter()
        .map(|r| ChunkKey::from_stored(r.key).is_some_and(|k| holds(chunker, lines, r.ordinal, r.window, k)))
        .collect()
}

/// semantic_resolver.rs `search_book` (recompute path): the line within
/// [`RECOMPUTE_REACH`] of `hint` that holds `key` as `window`, nearest first (ties to
/// the lower line). What a hit whose line moved (an insertion above it) resolves to.
pub fn locate(chunker: &Chunker, lines: &[LineRef<'_>], hint: usize, window: u8, key: &[u8]) -> Option<usize> {
    let key = ChunkKey::from_stored(key)?;
    if lines.is_empty() {
        return None;
    }
    let centre = hint.min(lines.len() - 1);
    let start = centre.saturating_sub(RECOMPUTE_REACH);
    let end = (centre + RECOMPUTE_REACH + 1).min(lines.len());
    let mut found: Vec<usize> = (start..end).filter(|&p| holds(chunker, lines, p, window, key)).collect();
    found.sort_by_key(|&p| (p.abs_diff(centre), p));
    found.first().copied()
}

// ---------------------------------------------------------------------------
// Book text (line_source.rs read_book_for_indexing_buffered, prepare_row;
// search_engine.rs prepare_text_book_fast)
// ---------------------------------------------------------------------------

/// One `line_note` row (inline_notes.rs `BookNote`), with the `lineIndex` of its line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookNote {
    /// `line.lineIndex` of the note's line (the SQL key of the notes query).
    pub line_index: i64,
    /// UTF-16 offset in the stored line, an entity counting one; -1 if absent.
    pub char_pos: i64,
    pub has_marker: bool,
    /// The body HTML (non-NULL bodies only, in `lineIndex, noteIndex` order).
    pub body: String,
}

/// One line of a book as Otzaria indexes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookLine {
    /// The row (index into the rows given, `lineIndex, id` order) the line comes from.
    pub row: usize,
    /// Which `\n`-separated piece of that row (0 for a row without `\n`).
    pub sub: usize,
    /// The normalized text (search_engine.rs `normalize_line_for_indexing(..).0`).
    pub text: String,
    /// The section: the number of `<h` lines up to and including this one.
    pub section: u64,
}

impl BookLine {
    pub fn as_ref(&self) -> LineRef<'_> {
        LineRef { text: &self.text, section: self.section }
    }
}

const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// The book's lines, ordinal = index, from its rows' raw content (`line_content.content`
/// or `line.content`, decoded from zstd by the host; NULL as empty) in `lineIndex, id`
/// order, and its `line_note` rows (none for a database without the table). Ports
/// line_source.rs `read_book_for_indexing_buffered` (`with_notes`), then the line split
/// and section rule of search_engine.rs `prepare_text_book_fast` (lines 8858, 8910):
/// rows joined with `\n` and split again, so a row holding `\n` gives several lines.
pub fn book_lines(rows: &[&[u8]], notes: &[BookNote]) -> Vec<BookLine> {
    let has_notes = !notes.is_empty();
    let mut next_note = 0usize;
    let mut text = String::new();
    let mut first_row_bom = false;
    let mut has_data_uri = false;
    let mut text_line = 0i64;
    // Where each row starts in `text`, to map lines back to rows.
    let mut row_of_line: Vec<(usize, usize)> = Vec::new();
    for (r, &raw_bytes) in rows.iter().enumerate() {
        if r > 0 {
            text.push('\n');
        }
        let mut bytes: std::borrow::Cow<[u8]> = raw_bytes.into();
        if has_notes {
            let raw = String::from_utf8_lossy(raw_bytes);
            let row_text: &str = if r == 0 { raw.strip_prefix('\u{FEFF}').unwrap_or(&raw) } else { &raw };
            let next_line = text_line + 1 + row_text.matches('\n').count() as i64;
            let mut book_notes: HashMap<i64, Vec<NoteRef>> = HashMap::new();
            while next_note < notes.len() && notes[next_note].line_index < next_line {
                let n = &notes[next_note];
                if n.line_index >= text_line {
                    book_notes.entry(n.line_index).or_default().push(NoteRef {
                        char_pos: n.char_pos,
                        has_marker: n.has_marker,
                        body: &n.body,
                    });
                }
                next_note += 1;
            }
            let rehydrated = rehydrate_row(row_text, text_line, &book_notes);
            text_line = next_line;
            let replaced = match rehydrated {
                Some(s) => Some(s.into_bytes()),
                None if row_text.len() != raw.len() => Some(row_text.as_bytes().to_vec()),
                None => None,
            };
            if let Some(replaced) = replaced {
                bytes = replaced.into();
            }
        }
        if bytes.windows(5).any(|w| w == b"data:") {
            has_data_uri = true;
        }
        first_row_bom |= r == 0 && bytes.starts_with(BOM);
        let prepared = strip_data_uris_for_index(&String::from_utf8_lossy(&bytes));
        for sub in 0..=prepared.matches('\n').count() {
            row_of_line.push((r, sub));
        }
        text.push_str(&prepared);
    }
    if rows.is_empty() {
        return Vec::new();
    }
    let text: &str = if first_row_bom && has_data_uri { &text[BOM.len()..] } else { &text };
    let mut section = 0u64;
    text.split('\n')
        .zip(row_of_line)
        .map(|(raw_line, (row, sub))| {
            if raw_line.starts_with("<h") {
                section += 1;
            }
            BookLine { row, sub, text: normalize_text_for_indexing(raw_line), section }
        })
        .collect()
}

/// line_source.rs `strip_data_uris_for_index`: every `data:` with at least 64 payload
/// characters is removed with its payload.
pub fn strip_data_uris_for_index(text: &str) -> String {
    const SCHEME: &str = "data:";
    const MIN_PAYLOAD: usize = 64;
    let is_payload = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b';' | b',' | b'=' | b'.' | b'-');
    let bytes = text.as_bytes();
    let Some(mut match_start) = text.find(SCHEME) else {
        return text.to_string();
    };
    let mut out = String::new();
    let mut copied_up_to = 0;
    loop {
        let mut end = match_start + SCHEME.len();
        while end < bytes.len() && is_payload(bytes[end]) {
            end += 1;
        }
        if end - match_start - SCHEME.len() >= MIN_PAYLOAD {
            out.push_str(&text[copied_up_to..match_start]);
            copied_up_to = end;
        }
        match text[end..].find(SCHEME) {
            Some(next) => match_start = end + next,
            None => break,
        }
    }
    if copied_up_to == 0 {
        return text.to_string();
    }
    out.push_str(&text[copied_up_to..]);
    out
}

// ---------------------------------------------------------------------------
// Footnotes put back (inline_notes.rs)
// ---------------------------------------------------------------------------

struct NoteRef<'a> {
    char_pos: i64,
    has_marker: bool,
    body: &'a str,
}

const JS_SPACE: &str = r"[\t\n\x0B\x0C\r \u{00A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]";
const B: &str = r"(?-u:\b)";

struct NoteRegexes {
    body: Regex,
    sup: Regex,
    marker: Regex,
    body_start: Regex,
}

fn note_regexes() -> &'static NoteRegexes {
    static R: OnceLock<NoteRegexes> = OnceLock::new();
    R.get_or_init(|| NoteRegexes {
        body: Regex::new(&format!(
            r#"(?s)<(?i-u:i){B}[^>]*{B}(?i-u:class){JS_SPACE}*={JS_SPACE}*"[^"]*{B}(?i-u:footnote){B}[^"]*"[^>]*>(.*?)</(?i-u:i)>"#
        ))
        .unwrap(),
        sup: Regex::new(&format!(r"(?s)<(?i-u:sup){B}[^>]*>.*?</(?i-u:sup)>")).unwrap(),
        marker: Regex::new(&format!(r"(?s)<(?i-u:sup){B}[^>]*{B}(?i-u:footnote-marker){B}[^>]*>.*?</(?i-u:sup)>")).unwrap(),
        body_start: Regex::new(&format!(
            r#"^{JS_SPACE}*<(?i-u:i){B}[^>]*{B}(?i-u:class){JS_SPACE}*={JS_SPACE}*"[^"]*{B}(?i-u:footnote){B}"#
        ))
        .unwrap(),
    })
}

/// inline_notes.rs `has_rehydrated_body`.
fn has_rehydrated_body(html: &str, sup_ranges: &[std::ops::Range<usize>]) -> bool {
    let mut bodies = note_regexes().body.find_iter(html).peekable();
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

/// inline_notes.rs `INLINE_TAG_NAMES`.
const INLINE_TAG_NAMES: &[&str] = &[
    "b", "i", "em", "strong", "u", "s", "strike", "span", "sup", "sub", "small", "big", "font", "del", "ins", "a",
    "abbr", "acronym", "cite", "code", "dfn", "kbd", "q", "samp", "tt", "var", "mark", "bdi", "bdo", "ruby", "rt", "rp",
];

/// inline_notes.rs `stripped_markers`.
fn stripped_markers(html: &str) -> Vec<std::ops::Range<usize>> {
    let r = note_regexes();
    r.marker.find_iter(html).filter(|m| !r.body_start.is_match(&html[m.end()..])).map(|m| m.range()).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Insertion {
    at: usize,
    splits_char: bool,
}

/// inline_notes.rs `raw_insertion_point`.
fn raw_insertion_point(html: &str, visible: i64) -> Insertion {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let end_of_line = Insertion { at: len, splits_char: false };
    let find = |from: usize, byte: u8| bytes[from..].iter().position(|&b| b == byte).map(|p| p + from);
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
                let ch = html[i..].chars().next().unwrap();
                if ch.len_utf16() == 2 {
                    if seen + 1 == visible {
                        return Insertion { at: i, splits_char: true };
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
    Insertion { at: after_inline_closings(html, i), splits_char: false }
}

/// inline_notes.rs `after_inline_closings`.
fn after_inline_closings(html: &str, mut at: usize) -> usize {
    while html[at..].starts_with("</") {
        let Some(close) = html[at..].find('>').map(|c| at + c) else {
            break;
        };
        let name = html[at + 2..close].trim_end();
        if !INLINE_TAG_NAMES.iter().any(|t| name.eq_ignore_ascii_case(t)) {
            break;
        }
        at = close + 1;
    }
    at
}

/// inline_notes.rs `only_tags_between`.
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

/// inline_notes.rs `rehydrate_inline_notes`.
fn rehydrate_inline_notes(line: &str, notes: &[NoteRef<'_>]) -> Option<String> {
    if notes.is_empty() {
        return None;
    }
    let sups: Vec<_> = note_regexes().sup.find_iter(line).map(|s| s.range()).collect();
    if has_rehydrated_body(line, &sups) {
        return None;
    }
    let markers = stripped_markers(line);
    let mut anchored_seen = 0;
    let mut previous_position: Option<(i64, Insertion)> = None;
    let mut inserts: Vec<(Insertion, usize)> = Vec::with_capacity(notes.len());
    for (order, note) in notes.iter().enumerate() {
        let marker = if note.has_marker { markers.get(anchored_seen) } else { None };
        let mut at = if note.char_pos >= 0 {
            let at = raw_insertion_point(line, note.char_pos);
            let inside = sups.get(sups.partition_point(|s| s.end <= at.at)).filter(|s| s.start < at.at);
            let empty_marker = marker.filter(|m| at.at <= m.start && only_tags_between(line, at.at, m.start));
            match inside.or(empty_marker) {
                Some(sup) => Insertion { at: after_inline_closings(line, sup.end), splits_char: false },
                None => at,
            }
        } else if let Some(marker) = marker {
            Insertion { at: marker.end, splits_char: false }
        } else {
            Insertion { at: line.len(), splits_char: false }
        };
        if note.char_pos >= 0 {
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
    let mut out = String::with_capacity(line.len() + notes.iter().map(|n| n.body.len() + 30).sum::<usize>());
    let mut last = 0;
    for (n, &(insert, order)) in inserts.iter().enumerate() {
        out.push_str(&line[last..insert.at]);
        last = insert.at;
        if insert.splits_char && (n == 0 || inserts[n - 1].0 != insert) {
            out.push('\u{FFFD}');
        }
        out.push_str("<i class=\"footnote\">");
        out.push_str(notes[order].body);
        out.push_str("</i>");
        if insert.splits_char && inserts.get(n + 1).map(|i| i.0) != Some(insert) {
            out.push('\u{FFFD}');
            last += line[insert.at..].chars().next().map_or(0, char::len_utf8);
        }
    }
    out.push_str(&line[last..]);
    Some(out)
}

/// inline_notes.rs `rehydrate_row`.
fn rehydrate_row(row: &str, first_line: i64, notes: &HashMap<i64, Vec<NoteRef<'_>>>) -> Option<String> {
    if !row.contains('\n') {
        return rehydrate_inline_notes(row, notes.get(&first_line)?);
    }
    let mut changed = false;
    let lines: Vec<std::borrow::Cow<str>> = row
        .split('\n')
        .enumerate()
        .map(|(n, line)| match notes.get(&(first_line + n as i64)).and_then(|ns| rehydrate_inline_notes(line, ns)) {
            Some(s) => {
                changed = true;
                s.into()
            }
            None => line.into(),
        })
        .collect();
    changed.then(|| lines.join("\n"))
}

// ---------------------------------------------------------------------------
// Line normalization (hebrew_query.rs normalize_text_for_indexing)
// ---------------------------------------------------------------------------

/// hebrew_query.rs `normalize_text_for_indexing` (= `normalize_one_pass(input, true, ..)`):
/// HTML stripped (breaking tags become a space), presentation forms folded, attached
/// marks dropped, whitespace collapsed and trimmed. LINE_TEXT_VERSION 4.
pub fn normalize_text_for_indexing(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut pending_space = false;
    let mut push = |c: char, out: &mut String| {
        if is_attached_mark(c) {
            return;
        }
        if c.is_whitespace() {
            if !out.is_empty() {
                pending_space = true;
            }
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    };
    for_each_html_stripped_piece(input, |piece| {
        for c in piece.chars() {
            match fold_presentation_form(c) {
                Some(folded) => folded.chars().for_each(|f| push(f, &mut out)),
                None => push(c, &mut out),
            }
        }
    });
    out
}

/// hebrew_query.rs `is_attached_mark`.
fn is_attached_mark(c: char) -> bool {
    matches!(c, '\u{0591}'..='\u{05C7}') && !matches!(c, '\u{05BE}' | '\u{05C0}' | '\u{05C3}' | '\u{05C6}')
}

/// hebrew_query.rs `is_word_mark`.
fn is_word_mark(c: char) -> bool {
    is_attached_mark(c) || matches!(c, '\u{0300}'..='\u{036F}') || c == '\u{FB1E}'
}

/// hebrew_query.rs `is_invisible_char`.
fn is_invisible_char(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
}

/// hebrew_query.rs `fold_presentation_form`.
fn fold_presentation_form(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{FB1D}' => "\u{05D9}\u{05B4}",
        '\u{FB1E}' => "",
        '\u{FB1F}' => "\u{05F2}\u{05B7}",
        '\u{FB20}' => "\u{05E2}",
        '\u{FB21}' => "\u{05D0}",
        '\u{FB22}' => "\u{05D3}",
        '\u{FB23}' => "\u{05D4}",
        '\u{FB24}' => "\u{05DB}",
        '\u{FB25}' => "\u{05DC}",
        '\u{FB26}' => "\u{05DD}",
        '\u{FB27}' => "\u{05E8}",
        '\u{FB28}' => "\u{05EA}",
        '\u{FB2A}' => "\u{05E9}\u{05C1}",
        '\u{FB2B}' => "\u{05E9}\u{05C2}",
        '\u{FB2C}' => "\u{05E9}\u{05BC}\u{05C1}",
        '\u{FB2D}' => "\u{05E9}\u{05BC}\u{05C2}",
        '\u{FB2E}' => "\u{05D0}\u{05B7}",
        '\u{FB2F}' => "\u{05D0}\u{05B8}",
        '\u{FB30}' => "\u{05D0}\u{05BC}",
        '\u{FB31}' => "\u{05D1}\u{05BC}",
        '\u{FB32}' => "\u{05D2}\u{05BC}",
        '\u{FB33}' => "\u{05D3}\u{05BC}",
        '\u{FB34}' => "\u{05D4}\u{05BC}",
        '\u{FB35}' => "\u{05D5}\u{05BC}",
        '\u{FB36}' => "\u{05D6}\u{05BC}",
        '\u{FB38}' => "\u{05D8}\u{05BC}",
        '\u{FB39}' => "\u{05D9}\u{05BC}",
        '\u{FB3A}' => "\u{05DA}\u{05BC}",
        '\u{FB3B}' => "\u{05DB}\u{05BC}",
        '\u{FB3C}' => "\u{05DC}\u{05BC}",
        '\u{FB3E}' => "\u{05DE}\u{05BC}",
        '\u{FB40}' => "\u{05E0}\u{05BC}",
        '\u{FB41}' => "\u{05E1}\u{05BC}",
        '\u{FB43}' => "\u{05E3}\u{05BC}",
        '\u{FB44}' => "\u{05E4}\u{05BC}",
        '\u{FB46}' => "\u{05E6}\u{05BC}",
        '\u{FB47}' => "\u{05E7}\u{05BC}",
        '\u{FB48}' => "\u{05E8}\u{05BC}",
        '\u{FB49}' => "\u{05E9}\u{05BC}",
        '\u{FB4A}' => "\u{05EA}\u{05BC}",
        '\u{FB4B}' => "\u{05D5}\u{05B9}",
        '\u{FB4C}' => "\u{05D1}\u{05BF}",
        '\u{FB4D}' => "\u{05DB}\u{05BF}",
        '\u{FB4E}' => "\u{05E4}\u{05BF}",
        '\u{FB4F}' => "\u{05D0}\u{05DC}",
        _ => return None,
    })
}

const SPACE_ENTITIES: [&[u8]; 4] = [b"&nbsp;", b"&thinsp;", b"&ensp;", b"&emsp;"];

fn space_entity_len(at: &[u8]) -> Option<usize> {
    SPACE_ENTITIES.iter().find(|e| at.starts_with(e)).map(|e| e.len())
}

/// hebrew_query.rs `for_each_html_stripped_piece_at`.
fn for_each_html_stripped_piece(text: &str, mut f: impl FnMut(&str)) {
    if !text.contains(['<', '&']) {
        f(text);
        return;
    }
    let bytes = text.as_bytes();
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        let (replacement, next) = match bytes[i] {
            b'<' => match bytes[i + 1..].iter().position(|&b| b == b'>') {
                Some(close) => (if tag_breaks_word(text, i, i + 1 + close) { " " } else { "" }, i + close + 2),
                None => ("<", i + 1),
            },
            b'&' => match space_entity_len(&bytes[i..]) {
                Some(len) => (" ", i + len),
                None => {
                    let mut j = i + 1;
                    let mut semi = None;
                    while j < bytes.len() {
                        match bytes[j] {
                            b';' => {
                                semi = Some(j);
                                break;
                            }
                            b'&' => j += space_entity_len(&bytes[j..]).unwrap_or(1),
                            _ => j += 1,
                        }
                    }
                    match semi {
                        Some(semi) if semi > i + 1 => ("", semi + 1),
                        _ => ("&", i + 1),
                    }
                }
            },
            _ => {
                i += 1;
                continue;
            }
        };
        if copied < i {
            f(&text[copied..i]);
        }
        if !replacement.is_empty() {
            f(replacement);
        }
        i = next;
        copied = next;
    }
    if copied < bytes.len() {
        f(&text[copied..]);
    }
}

/// hebrew_query.rs `BREAKING_TAG_NAMES`.
const BREAKING_TAG_NAMES: &[&str] = &[
    "address", "article", "aside", "blockquote", "br", "caption", "center", "dd", "div", "dl", "dt", "figcaption",
    "figure", "footer", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hr", "li", "main", "nav", "ol", "p", "pre",
    "section", "table", "tbody", "td", "tfoot", "th", "thead", "tr", "ul",
];

/// hebrew_query.rs `SCRIPT_TAG_NAMES`.
const SCRIPT_TAG_NAMES: &[&str] = &["sub", "sup"];

/// hebrew_query.rs `tag_name`.
fn tag_name(body: &str) -> Option<(bool, &str)> {
    let (closing, name) = match body.strip_prefix('/') {
        Some(name) => (true, name),
        None => (false, body),
    };
    let len = name.bytes().take_while(u8::is_ascii_alphanumeric).count();
    (1..=10).contains(&len).then(|| (closing, &name[..len]))
}

fn is_listed(names: &[&str], name: &str) -> bool {
    names.iter().any(|n| n.eq_ignore_ascii_case(name))
}

fn is_hebrew_letter(c: char) -> bool {
    matches!(c, '\u{05D0}'..='\u{05EA}')
}

/// hebrew_query.rs `is_script_letter`.
fn is_script_letter(c: char) -> bool {
    match fold_presentation_form(c) {
        Some(folded) => {
            let mut folded = folded.chars();
            folded.next().is_some_and(is_hebrew_letter) && folded.all(is_attached_mark)
        }
        None => is_hebrew_letter(c),
    }
}

/// hebrew_query.rs `tag_breaks_word`.
fn tag_breaks_word(source: &str, lt: usize, gt: usize) -> bool {
    let Some((closing, name)) = tag_name(&source[lt + 1..gt]) else {
        return false;
    };
    if is_listed(BREAKING_TAG_NAMES, name) {
        return true;
    }
    if !is_listed(SCRIPT_TAG_NAMES, name) {
        return false;
    }
    let open = if closing { script_open_before(source, lt) } else { Some((lt, gt)) };
    !open.is_some_and(|(lt, gt)| is_word_internal_script(source, lt, gt))
}

/// hebrew_query.rs `script_open_before`.
fn script_open_before(source: &str, lt: usize) -> Option<(usize, usize)> {
    let mut before = trim_script_context_end(&source[..lt]).chars();
    before.next_back().filter(|&c| is_script_letter(c))?;
    let gt = trim_script_formatting_end(before.as_str()).len().checked_sub(1)?;
    if source.as_bytes()[gt] != b'>' {
        return None;
    }
    let lt = source[..gt].rfind('<')?;
    (!source[lt + 1..gt].contains('>')).then_some((lt, gt))
}

/// hebrew_query.rs `is_inline_tag`.
fn is_inline_tag(body: &str) -> bool {
    !tag_name(body).is_some_and(|(_, n)| is_listed(BREAKING_TAG_NAMES, n) || is_listed(SCRIPT_TAG_NAMES, n))
}

/// hebrew_query.rs `is_script_invisible`.
fn is_script_invisible(c: char) -> bool {
    is_invisible_char(c) || c == '\u{034F}' || c == '\u{FB1E}'
}

/// hebrew_query.rs `trim_script_formatting_start`.
fn trim_script_formatting_start(mut source: &str) -> &str {
    loop {
        source = source.trim_start_matches(is_script_invisible);
        let Some(body) = source.strip_prefix('<') else { break };
        let Some(gt) = body.find('>') else { break };
        if !is_inline_tag(&body[..gt]) {
            break;
        }
        source = &body[gt + 1..];
    }
    source
}

/// hebrew_query.rs `trim_script_formatting_end`.
fn trim_script_formatting_end(mut source: &str) -> &str {
    loop {
        source = source.trim_end_matches(is_script_invisible);
        let Some(body) = source.strip_suffix('>') else { break };
        let Some(lt) = body.rfind('<') else { break };
        if body[lt + 1..].contains('>') || !is_inline_tag(&body[lt + 1..]) {
            break;
        }
        source = &body[..lt];
    }
    source
}

/// hebrew_query.rs `trim_script_context_end`.
fn trim_script_context_end(mut source: &str) -> &str {
    loop {
        let trimmed = trim_script_formatting_end(source).trim_end_matches(is_word_mark);
        if trimmed.len() == source.len() {
            return source;
        }
        source = trimmed;
    }
}

/// hebrew_query.rs `trim_script_context_start`.
fn trim_script_context_start(mut source: &str) -> &str {
    loop {
        let trimmed = trim_script_formatting_start(source).trim_start_matches(is_word_mark);
        if trimmed.len() == source.len() {
            return source;
        }
        source = trimmed;
    }
}

/// hebrew_query.rs `marks_footnote`.
fn marks_footnote(attributes: &str) -> bool {
    let lower = attributes.to_ascii_lowercase();
    let word = |b: Option<&u8>| b.is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
    lower.match_indices("footnote-marker").any(|(i, found)| {
        !word(lower.as_bytes()[..i].last()) && !word(lower.as_bytes().get(i + found.len()))
    })
}

/// hebrew_query.rs `is_word_internal_script`.
fn is_word_internal_script(source: &str, lt: usize, gt: usize) -> bool {
    let Some((false, name)) = tag_name(&source[lt + 1..gt]) else {
        return false;
    };
    if marks_footnote(&source[lt + 1..gt]) {
        return false;
    }
    if !trim_script_context_end(&source[..lt]).chars().next_back().is_some_and(is_script_letter) {
        return false;
    }
    let mut content = trim_script_formatting_start(&source[gt + 1..]).chars();
    if !content.next().is_some_and(is_script_letter) {
        return false;
    }
    let rest = trim_script_context_start(content.as_str());
    let Some(close) = rest.strip_prefix('<') else {
        return false;
    };
    let Some(close_gt) = close.find('>') else {
        return false;
    };
    matches!(tag_name(&close[..close_gt]), Some((true, c)) if c.eq_ignore_ascii_case(name))
        && trim_script_context_start(&close[close_gt + 1..]).chars().next().is_some_and(is_script_letter)
}

#[cfg(test)]
#[path = "chunker_tests.rs"]
pub(crate) mod tests;
