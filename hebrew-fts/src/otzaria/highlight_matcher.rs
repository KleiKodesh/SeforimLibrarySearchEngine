// Ported verbatim from otzaria_search_engine 7.2.0 (MIT, Copyright (c) 2026 otzaria),
// rust/src/highlight_matcher.rs; see THIRD-PARTY-NOTICES.md. Changes: module paths, the test
// module dropped (golden tests live in hebrew-fts/tests/otzaria_helpers.rs).
//! Prepared, bounded display matching. Regexes only inspect individual index
//! tokens; phrase gaps are resolved with dynamic programming, never regex
//! backtracking. Work is O(words * tokens), plus linear token-regex matching.
//! Any-order phrases use the engine's own window search ([`WordWindows`]).

use std::collections::HashMap;
use std::sync::Arc;

use regex::{CaptureLocations, Regex, RegexBuilder};
use tantivy::tokenizer::{TokenStream, Tokenizer};

use crate::otzaria::gap_phrase::{Proximity, WordWindows};
use crate::otzaria::hebrew_query::{fold_presentation_form, is_word_mark, tag_breaks_word};
use crate::otzaria::hebrew_tokenizer::{is_geresh, is_gershayim, is_transparent, HebrewTokenizer};

pub(crate) struct PreparedHighlightMatcher {
    // Indexed by the outer left word's `Edge` + 3 * the outer right word's.
    words: Vec<[Arc<Regex>; 9]>,
    independent_words: Vec<[Arc<Regex>; 3]>,
    proximity: Proximity,
    boundary_eligible: Vec<bool>,
    groups: Vec<Self>,
    cross_line: bool,
}

/// How an outer word may sit in its token.
struct Edge;

impl Edge {
    const WHOLE: usize = 0;
    const AFFIXES: usize = 1;
    const ANY: usize = 2;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Range {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Match {
    pub start: usize,
    pub end: usize,
    /// Relative to `start`, in Dart/JavaScript UTF-16 code units.
    pub word_ranges: Vec<Range>,
}

impl PreparedHighlightMatcher {
    pub(crate) fn new(
        branches: Vec<Vec<(String, String, String)>>,
        proximity: Proximity,
        boundary_eligible: Vec<bool>,
    ) -> Result<Self, regex::Error> {
        let mut cache = HashMap::<String, Arc<Regex>>::new();
        let word_count = branches.len();
        // Any order has no outer words: each keeps its own affixes, exactly as
        // the engine's per-word pattern does, whatever the boundary request.
        let edges = !proximity.is_any_order();
        let mut words = Vec::with_capacity(word_count);
        let mut independent_words = Vec::with_capacity(word_count);
        let mut compile = |pattern: String| -> Result<Arc<Regex>, regex::Error> {
            if let Some(regex) = cache.get(&pattern) {
                return Ok(regex.clone());
            }
            let regex = Arc::new(RegexBuilder::new(&pattern).case_insensitive(true).build()?);
            cache.insert(pattern, regex.clone());
            Ok(regex)
        };
        for (i, branches) in branches.into_iter().enumerate() {
            let roots: Vec<String> = branches
                .iter()
                .map(|(_, root, _)| format!("({root})"))
                .collect();
            let roots = roots.join("|");
            let affixed: Vec<String> = branches
                .iter()
                .map(|(lead, root, trail)| format!("(?:{lead})({root})(?:{trail})"))
                .collect();
            // An expanded word's waiver admits its own affixes, never any
            // letters: "שבת" with prefixes alone does not paint "מחשבתו".
            independent_words.push([
                compile(format!("\\A(?:{roots})\\z"))?,
                compile(format!("\\A(?:{})\\z", affixed.join("|")))?,
                compile(format!("(?:{roots})"))?,
            ]);
            let mut modes = Vec::with_capacity(9);
            for mode in 0..9 {
                let alternatives: Vec<String> = branches
                    .iter()
                    .map(|(lead, root, trail)| {
                        let lead = match (edges && i == 0).then_some(mode % 3) {
                            Some(Edge::WHOLE) => "",
                            Some(Edge::ANY) => ".*?",
                            _ => lead.as_str(),
                        };
                        let trail = match (edges && i + 1 == word_count).then_some(mode / 3) {
                            Some(Edge::WHOLE) => "",
                            Some(Edge::ANY) => ".*",
                            _ => trail.as_str(),
                        };
                        format!("(?:{lead})({root})(?:{trail})")
                    })
                    .collect();
                modes.push(compile(format!("\\A(?:{})\\z", alternatives.join("|")))?);
            }
            words.push(modes.try_into().unwrap());
        }
        Ok(Self {
            words,
            independent_words,
            proximity,
            boundary_eligible,
            groups: Vec::new(),
            cross_line: true,
        })
    }

    /// Each group remains a complete phrase, even when the caller asks for word matches.
    pub(crate) fn from_groups(groups: Vec<Self>, cross_line: bool) -> Self {
        Self {
            words: Vec::new(),
            independent_words: Vec::new(),
            proximity: Proximity::Ordered(Vec::new()),
            boundary_eligible: groups
                .iter()
                .flat_map(|group| group.boundary_eligible.iter().copied())
                .collect(),
            groups,
            cross_line,
        }
    }

    pub(crate) fn allows_cross_line(&self) -> bool {
        self.cross_line
    }

    fn find_group_matches(
        &self,
        data: &str,
        require_boundaries: &[bool],
        require_all: bool,
    ) -> Vec<Match> {
        let mut offset = 0;
        let mut matches = Vec::new();
        for group in &self.groups {
            let end = offset + group.boundary_eligible.len();
            let found =
                group.find_matches(data, require_boundaries.get(offset..end).unwrap_or(&[]));
            if require_all && found.is_empty() {
                return Vec::new();
            }
            matches.extend(found);
            offset = end;
        }
        matches.sort_unstable_by_key(|matched| (matched.start, matched.end));
        let mut merged: Vec<Match> = Vec::new();
        for matched in matches {
            if let Some(last) = merged.last_mut().filter(|last| matched.start <= last.end) {
                let shift = matched.start - last.start;
                last.end = last.end.max(matched.end);
                last.word_ranges
                    .extend(matched.word_ranges.into_iter().map(|range| Range {
                        start: shift + range.start,
                        end: shift + range.end,
                    }));
                last.word_ranges
                    .sort_unstable_by_key(|range| (range.start, range.end));
                let mut ranges: Vec<Range> = Vec::new();
                for range in last.word_ranges.drain(..) {
                    if let Some(previous) = ranges.last_mut().filter(|old| range.start <= old.end) {
                        previous.end = previous.end.max(range.end);
                    } else {
                        ranges.push(range);
                    }
                }
                last.word_ranges = ranges;
            } else {
                merged.push(matched);
            }
        }
        merged
    }

    pub(crate) fn find_matches(&self, data: &str, require_boundaries: &[bool]) -> Vec<Match> {
        if !self.groups.is_empty() {
            return self.find_group_matches(data, require_boundaries, true);
        }
        if self.words.is_empty() || data.is_empty() {
            return Vec::new();
        }
        if self.words.len() == 1 {
            return self.find_word_matches(data, require_boundaries);
        }
        let text = MappedText::strip_markup(data);
        let mut tokenizer = HebrewTokenizer {
            emit_quote_free: true,
            keep_marks: false,
        };
        let mut stream = tokenizer.token_stream(&text.text);
        let mut candidates: Vec<Vec<Option<Range>>> = vec![Vec::new(); self.words.len()];
        let mut mapped = MappedText::default();
        let mut plans: Vec<(&Regex, CaptureLocations)> = (0..self.words.len())
            .map(|i| {
                let edge = self.edge(i, require_boundaries);
                let left = if i == 0 { edge } else { Edge::WHOLE };
                let right = if i + 1 == self.words.len() {
                    edge
                } else {
                    Edge::WHOLE
                };
                let regex = &*self.words[i][left + 3 * right];
                (regex, regex.capture_locations())
            })
            .collect();
        while stream.advance() {
            let token = stream.token();
            let position = token.position;
            if candidates[0].len() <= position {
                for row in &mut candidates {
                    row.resize(position + 1, None);
                }
            }
            text.normalized_token(
                token.offset_from,
                token.offset_to,
                !token.text.contains(['\'', '"']),
                &mut mapped,
            );
            for (i, (regex, locs)) in plans.iter_mut().enumerate() {
                if candidates[i][position].is_some() {
                    continue;
                }
                if regex.captures_read(locs, &mapped.text).is_some() {
                    if let Some((start, end)) = first_group(locs) {
                        if start < end {
                            candidates[i][position] = Some(Range {
                                start: mapped.starts[start],
                                end: mapped.ends[end - 1],
                            });
                        }
                    }
                }
            }
        }
        self.resolve(candidates)
    }

    pub(crate) fn find_word_matches(&self, data: &str, require_boundaries: &[bool]) -> Vec<Match> {
        if !self.groups.is_empty() {
            return self.find_group_matches(data, require_boundaries, false);
        }
        let text = MappedText::strip_markup(data);
        let mut tokenizer = HebrewTokenizer {
            emit_quote_free: true,
            keep_marks: false,
        };
        let mut stream = tokenizer.token_stream(&text.text);
        let mut ranges = Vec::new();
        let mut mapped = MappedText::default();
        let mut plans: Vec<(&Regex, usize, CaptureLocations)> = self
            .independent_words
            .iter()
            .enumerate()
            .map(|(i, patterns)| {
                let edge = self.edge(i, require_boundaries);
                (&*patterns[edge], edge, patterns[edge].capture_locations())
            })
            .collect();
        while stream.advance() {
            let token = stream.token();
            text.normalized_token(
                token.offset_from,
                token.offset_to,
                !token.text.contains(['\'', '"']),
                &mut mapped,
            );
            for (regex, edge, locs) in &mut plans {
                let haystack = mapped.text.as_str();
                let mut at = 0;
                let mut last_end = None;
                // Only a waived plain word may occur more than once in a token.
                while let Some(mut found) = regex.captures_read_at(locs, haystack, at) {
                    // As in `captures_iter`: skip an empty match touching the previous one.
                    if found.is_empty() && Some(found.end()) == last_end {
                        if at + 1 > haystack.len() {
                            break;
                        }
                        let Some(next) = regex.captures_read_at(locs, haystack, at + 1) else {
                            break;
                        };
                        found = next;
                    }
                    let Some((start, end)) = first_group(locs) else {
                        break;
                    };
                    if start == end {
                        break;
                    }
                    ranges.push(Range {
                        start: mapped.starts[start],
                        end: mapped.ends[end - 1],
                    });
                    if *edge != Edge::ANY {
                        break;
                    }
                    at = found.end();
                    last_end = Some(found.end());
                }
            }
        }
        ranges.sort_by_key(|r| (r.start, r.end));
        let mut matches = Vec::new();
        let mut previous_end = 0;
        for range in ranges {
            if range.start < previous_end {
                continue;
            }
            previous_end = range.end;
            matches.push(Match {
                start: range.start,
                end: range.end,
                word_ranges: vec![Range {
                    start: 0,
                    end: range.end - range.start,
                }],
            });
        }
        matches
    }

    /// A caller waiving a plain word's boundary asks for the root anywhere in
    /// the token: partial-word display, or a quote cut mid-word.
    fn edge(&self, word: usize, require_boundaries: &[bool]) -> usize {
        let eligible = self.boundary_eligible[word];
        match require_boundaries.get(word).copied().unwrap_or(eligible) {
            true => Edge::WHOLE,
            false if eligible => Edge::ANY,
            false => Edge::AFFIXES,
        }
    }

    fn resolve(&self, candidates: Vec<Vec<Option<Range>>>) -> Vec<Match> {
        let token_count = candidates[0].len();
        if token_count == 0 {
            return Vec::new();
        }
        let gaps = match &self.proximity {
            Proximity::Ordered(gaps) => gaps,
            &Proximity::AnyOrder { max_span } => {
                return Self::resolve_any_order(&candidates, max_span)
            }
        };
        let word_count = candidates.len();
        // A successor is the earliest next position that can complete the
        // entire remaining phrase. A greedy choice without this viability
        // pass loses valid paths when a later word has a tighter gap.
        let mut successor = vec![vec![None; token_count]; word_count.saturating_sub(1)];
        let mut viable: Vec<bool> = candidates[word_count - 1]
            .iter()
            .map(Option::is_some)
            .collect();
        for i in (0..word_count.saturating_sub(1)).rev() {
            let mut next = vec![None; token_count + 1];
            for p in (0..token_count).rev() {
                next[p] = if viable[p] { Some(p) } else { next[p + 1] };
            }
            for (p, candidate) in candidates[i].iter().enumerate() {
                let max_next = p.saturating_add(gaps[i] as usize).saturating_add(1);
                successor[i][p] = if candidate.is_some() {
                    next[p + 1].filter(|&q| q <= max_next)
                } else {
                    None
                };
                viable[p] = successor[i][p].is_some();
            }
        }
        let mut matches = Vec::new();
        let mut previous_end = 0;
        for (p, &can_complete) in viable.iter().enumerate() {
            if !can_complete {
                continue;
            }
            let first = candidates[0][p].unwrap();
            if first.start < previous_end {
                continue;
            }
            let mut ranges = vec![first];
            let mut position = p;
            for i in 0..word_count.saturating_sub(1) {
                position = successor[i][position].unwrap();
                ranges.push(candidates[i + 1][position].unwrap());
            }
            let end = ranges.last().unwrap().end;
            previous_end = end;
            matches.push(Match {
                start: first.start,
                end,
                word_ranges: ranges
                    .into_iter()
                    .map(|r| Range {
                        start: r.start - first.start,
                        end: r.end - first.start,
                    })
                    .collect(),
            });
        }
        matches
    }

    /// Non-overlapping windows, each ending as early as possible, in which every
    /// word takes its own token position — the engine's any-order verifier.
    fn resolve_any_order(candidates: &[Vec<Option<Range>>], max_span: u32) -> Vec<Match> {
        let mut windows = WordWindows::default();
        windows.reset(candidates.len());
        let mut slots = Vec::new();
        for position in 0..candidates[0].len() {
            let Ok(at) = u32::try_from(position) else {
                break;
            };
            for (word, row) in candidates.iter().enumerate() {
                if let Some(range) = row[position] {
                    if windows.push(at, word).is_some() {
                        slots.push(range);
                    }
                }
            }
        }
        let mut matches = Vec::new();
        let mut assignment = Vec::new();
        let mut from = 0;
        while let Some(last) = windows.next_window(from, max_span, &mut assignment) {
            let mut ranges: Vec<Range> = assignment
                .iter()
                .enumerate()
                .map(|(word, &entry)| slots[windows.slot(entry, word)])
                .collect();
            ranges.sort_by_key(|r| (r.start, r.end));
            let start = ranges[0].start;
            let end = ranges.iter().map(|r| r.end).max().unwrap();
            matches.push(Match {
                start,
                end,
                word_ranges: ranges
                    .into_iter()
                    .map(|r| Range {
                        start: r.start - start,
                        end: r.end - start,
                    })
                    .collect(),
            });
            from = last + 1;
        }
        matches
    }
}

/// Each emitted UTF-8 byte maps to the original UTF-16 character range.
/// Token normalization may delete punctuation or expand a presentation form;
/// this mapping preserves exact display offsets through both operations.
#[derive(Default)]
struct MappedText {
    text: String,
    starts: Vec<usize>,
    ends: Vec<usize>,
}

impl MappedText {
    const MAX_INITIAL_CAPACITY: usize = 1024;

    fn push(&mut self, c: char, start: usize, end: usize) {
        self.text.push(c);
        self.starts.extend(std::iter::repeat_n(start, c.len_utf8()));
        self.ends.extend(std::iter::repeat_n(end, c.len_utf8()));
    }

    fn extend_last(&mut self, end: usize) {
        if let Some(c) = self.text.chars().next_back() {
            let n = self.ends.len();
            self.ends[n - c.len_utf8()..].fill(end);
        }
    }

    fn strip_markup(data: &str) -> Self {
        // Tags and entities can disappear entirely, so bound the reservation
        // based on raw input size. Longer visible text grows normally.
        let capacity = data.len().min(Self::MAX_INITIAL_CAPACITY);
        let mut out = Self {
            text: String::with_capacity(capacity),
            starts: Vec::with_capacity(capacity),
            ends: Vec::with_capacity(capacity),
        };
        let mut byte = 0;
        let mut utf16 = 0;
        let mut has_tag_end = true;
        let mut has_entity_end = true;
        while byte < data.len() {
            let rest = &data[byte..];
            let c = rest.chars().next().unwrap();
            if c == '<' && has_tag_end {
                if let Some(close) = rest.find('>') {
                    let end = utf16 + rest[..=close].encode_utf16().count();
                    if tag_breaks_word(data, byte, byte + close) {
                        out.push(' ', utf16, end);
                    }
                    byte += close + 1;
                    utf16 = end;
                    continue;
                }
                // Once no closing delimiter remains, never rescan the suffix
                // for each later '<'. Malformed markup stays linear too.
                has_tag_end = false;
            } else if c == '&' && has_entity_end {
                if let Some(close) = rest.find(';') {
                    if close <= 1 {
                        out.push(c, utf16, utf16 + c.len_utf16());
                        byte += c.len_utf8();
                        utf16 += c.len_utf16();
                        continue;
                    }
                    let end = utf16 + rest[..=close].encode_utf16().count();
                    if matches!(&rest[..=close], "&nbsp;" | "&thinsp;" | "&ensp;" | "&emsp;") {
                        out.push(' ', utf16, end);
                    }
                    byte += close + 1;
                    utf16 = end;
                    continue;
                }
                has_entity_end = false;
            }
            let end = utf16 + c.len_utf16();
            out.push(c, utf16, end);
            byte += c.len_utf8();
            utf16 = end;
        }
        out
    }

    /// Writes the token into `out`, reused across tokens to spare its allocations.
    fn normalized_token(&self, start: usize, end: usize, quote_free: bool, out: &mut Self) {
        out.text.clear();
        out.starts.clear();
        out.ends.clear();
        for (i, c) in self.text[start..end].char_indices() {
            let source_start = self.starts[start + i];
            let source_end = self.ends[start + i];
            if is_word_mark(c) {
                out.extend_last(source_end);
            } else if is_transparent(c) {
                continue;
            } else if is_geresh(c) || is_gershayim(c) {
                if quote_free {
                    continue;
                }
                let mut quote = if is_geresh(c) { '\'' } else { '"' };
                if quote == '\'' && out.text.ends_with('\'') {
                    let previous_start = out.starts.pop().unwrap();
                    out.ends.pop();
                    out.text.pop();
                    quote = '"';
                    out.push(quote, previous_start, source_end);
                } else if quote == '"' && out.text.ends_with('"') {
                    out.extend_last(source_end);
                } else {
                    out.push(quote, source_start, source_end);
                }
            } else if let Some(folded) = fold_presentation_form(c) {
                for f in folded.chars().filter(|c| !is_word_mark(*c)) {
                    out.push(f, source_start, source_end);
                }
            } else {
                out.push(c, source_start, source_end);
            }
        }
    }
}

/// The first participating capture group after the whole match.
fn first_group(locs: &CaptureLocations) -> Option<(usize, usize)> {
    (1..locs.len()).find_map(|group| locs.get(group))
}

