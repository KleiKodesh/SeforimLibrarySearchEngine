//! Otzaria's `Line` kind of user regex (`user_regex.rs`: `line_plan`,
//! `required_clauses`, `LineView`, `LinePlan::is_match` / `find`), ported
//! as Otzaria does it: a pattern that spans a space explicitly (an explicit
//! space class such as `[ ,]`, inner anchors or `\b`, a wildcard-only word in
//! a sequence) is matched on each line's TEXT, not on dictionary terms.
//!
//! - [`LinePlan`]: the regex with a word boundary implied on each edge that
//!   can be a word character, a prefilter without word-boundary assertions
//!   (the regex crate runs it as a DFA; a Unicode `\b` forces its slow
//!   engines), and the pattern's required fixed words as AND-ed clauses (an
//!   OR of fragments each). The engine turns the selective clauses into
//!   candidate lines from the index; the host supplies their texts.
//! - [`LineView`]: a line as the pattern sees it (Otzaria's app
//!   `cleanLineForSearch` plus the index foldings), with a map from every
//!   view byte to the UTF-16 range of the source it came from, so matches
//!   come back as UTF-16 ranges into the host's text (for highlighting).
//! - Line anchors (Otzaria 7.0.0): a pattern that asserts `^` (`\A`) or `$`
//!   (`\z`) also runs on the line without its leading (trailing) numbering
//!   (`crate::line_edges::content_range`: `(x) ` / `[x] ` at the start, `{x}`
//!   at the end), so `^` holds past an enumerator and `$` before a closing
//!   one; a `[x]` glued to the word is part of it and is not skipped. One
//!   match per anchor: where the pattern holds past the enumerator, the match
//!   that needs the anchor at the raw edge is dropped (Otzaria's `find`).

use regex::{Regex, RegexBuilder};
use regex_automata::{meta::Regex as LineRegex, Input};
use regex_syntax::hir::{Class, Hir, HirKind, Look, Repetition};

use crate::chars::{fold_presentation_form, is_indexable_len, is_invisible, is_quote, is_transparent, is_word_mark};
use crate::html_tags::tag_breaks_word;
use crate::scanner::skip_entity;
use crate::user_regex::{cost, literal_text};

/// Otzaria's `LINE_REGEX_SIZE_LIMIT`.
const LINE_REGEX_SIZE_LIMIT: usize = 8 << 20;
/// Otzaria's `REGEX_COST_LIMIT`.
const REGEX_COST_LIMIT: u64 = 20_000;

/// Otzaria's `CLAUSE_POSTINGS_CAP`: a clause whose terms cover more postings
/// than this narrows nothing worth reading (dropped).
pub const CLAUSE_POSTINGS_CAP: u64 = 3_000_000;
/// Otzaria's `CLAUSE_MAX_TERMS`: a partial fragment matching more terms than
/// this is not selective (dropped).
pub const CLAUSE_MAX_TERMS: usize = 50_000;
/// Otzaria's `MAX_LINE_HITS`: line hits kept by one search; more end it with
/// the truncated flag.
pub const MAX_LINE_HITS: usize = 100_000;

/// Otzaria's `BREAKING_TAG_NAMES` (`hebrew_query.rs`): tags the line view
/// turns into a space; every other tag vanishes.
const BREAKING_TAG_NAMES: &[&str] = &[
    "address", "article", "aside", "blockquote", "br", "caption", "center", "dd", "div", "dl", "dt",
    "figcaption", "figure", "footer", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hr", "li", "main",
    "nav", "ol", "p", "pre", "section", "table", "tbody", "td", "tfoot", "th", "thead", "tr", "ul",
];

/// A fixed word every match of a line pattern contains, as the index's
/// scanner makes it a term (Otzaria's `Fragment`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fragment {
    /// The term as indexed (lowercase, marks stripped), quotes removed.
    pub text: String,
    /// The line's term starts with `text` (a separator right before it);
    /// otherwise more letters may come first.
    pub start: bool,
    /// The line's term ends with `text`; otherwise more letters may follow.
    pub end: bool,
}

/// A compiled line pattern (Otzaria's `LinePlan`).
#[derive(Debug, Clone)]
pub struct LinePlan {
    regex: LineRegex,
    prefilter: Regex,
    /// Fixed words: every clause (an OR of fragments) holds in every matching line.
    pub clauses: Vec<Vec<Fragment>>,
    /// The pattern asserts `\A` (`\z`) somewhere: it also holds after a
    /// leading (before a trailing) enumerator such as `(x) `.
    starts_line: bool,
    ends_line: bool,
    /// Every matching path requires this anchor: no dependency search is needed.
    requires_start: bool,
    requires_end: bool,
}

impl LinePlan {
    /// Otzaria's `line_plan`, from the parsed top-level elements.
    pub fn new(elements: &[Hir]) -> Result<LinePlan, String> {
        let whole = Hir::concat(elements.to_vec());
        if cost(&whole) > REGEX_COST_LIMIT {
            return Err("too complex: bounded repetitions too large".to_string());
        }
        let pattern = whole.to_string();
        let (first_word, _) = edge_chars(&whole, true);
        let (last_word, _) = edge_chars(&whole, false);
        let source = format!(
            "{}(?:{pattern}){}",
            if first_word { r"\b{start-half}" } else { "" },
            if last_word { r"\b{end-half}" } else { "" },
        );
        let build = |source: &str| {
            RegexBuilder::new(source)
                .case_insensitive(true)
                .size_limit(LINE_REGEX_SIZE_LIMIT)
                .dfa_size_limit(LINE_REGEX_SIZE_LIMIT)
                .build()
                .map_err(|e| format!("too complex: {e}"))
        };
        let looks = whole.properties().look_set();
        Ok(LinePlan {
            regex: build_line_regex(&source)?,
            prefilter: build(&without_word_looks(&whole).to_string())?,
            clauses: required_clauses(elements),
            starts_line: looks.contains(Look::Start),
            ends_line: looks.contains(Look::End),
            requires_start: whole.properties().look_set_prefix().contains(Look::Start),
            requires_end: whole.properties().look_set_suffix().contains(Look::End),
        })
    }

    /// Otzaria's `LinePlan::edges`: `[0, content start, len, content end]`
    /// of `view`; the content edges stay at the line's edges unless the
    /// pattern is anchored there.
    fn edges(&self, view: &str) -> [usize; 4] {
        let mut edges = [0, 0, view.len(), view.len()];
        if self.starts_line || self.ends_line {
            let content = crate::line_edges::content_range(view);
            // The index keeps `[x]` glued to a word inside that word's term: skip it only spaced.
            let (lead, tail) = (&view[..content.start], &view[content.end..]);
            if self.starts_line && !lead.ends_with(']') {
                edges[1] = content.start;
            }
            if self.ends_line && !tail.starts_with('[') {
                edges[3] = content.end;
            }
        }
        edges
    }

    /// Otzaria's `LinePlan::windows`: the parts of `view` the pattern runs
    /// on: the whole line, and for an anchored pattern also the line without
    /// its edge enumerators, so `\A` and `\z` hold at either edge.
    fn windows(edges: [usize; 4]) -> impl Iterator<Item = std::ops::Range<usize>> {
        [(0, 2), (1, 2), (0, 3), (1, 3)].into_iter().filter_map(move |(s, e)| {
            let fresh = (s == 0 || edges[1] != edges[0]) && (e == 2 || edges[3] != edges[2]);
            (fresh && edges[s] < edges[e]).then(|| edges[s]..edges[e])
        })
    }

    /// True when the line view holds a non-empty match.
    pub fn is_match(&self, view: &str) -> bool {
        Self::windows(self.edges(view)).any(|w| {
            let part = &view[w];
            self.prefilter.is_match(part) && self.regex.find_iter(part).any(|m| !m.is_empty())
        })
    }

    /// Byte ranges of the non-empty matches in `view`, without overlaps
    /// (Otzaria's `LinePlan::find`).
    pub fn find(&self, view: &str) -> Vec<std::ops::Range<usize>> {
        use std::ops::Range;
        let edges = self.edges(view);
        if edges[0] == edges[1] && edges[2] == edges[3] {
            return self.regex.find_iter(view).filter(|m| !m.is_empty()).map(|m| m.range()).collect();
        }
        // Each match keeps its window, so each anchor can be tested on its
        // own without changing the text a consuming alternative sees.
        let mut found: Vec<(Range<usize>, Range<usize>)> = Self::windows(edges)
            .flat_map(|w| {
                self.regex
                    .find_iter(&view[w.clone()])
                    .filter(|m| !m.is_empty())
                    .map(move |m| (w.start + m.start()..w.start + m.end(), w.clone()))
            })
            .collect();
        let padded = format!("\n{view}\n");
        // True when the match `r` found in window `w` needs the anchor on
        // that side: the same search with only that absolute anchor disabled
        // (a sentinel line break past the window edge; the span keeps the
        // match inside the window) does not find it.
        let depends_on = |r: &Range<usize>, w: &Range<usize>, start: bool| {
            if (start && self.requires_start) || (!start && self.requires_end) {
                return true;
            }
            let m = if start {
                self.regex
                    .find(Input::new(&padded[..w.end + 1]).span(r.start + 1..w.end + 1))
                    .map(|m| m.start() - 1..m.end() - 1)
            } else {
                self.regex
                    .find(Input::new(&padded[w.start + 1..]).span(r.start - w.start..w.end - w.start))
                    .map(|m| w.start + m.start()..w.start + m.end())
            };
            m.as_ref() != Some(r)
        };
        // Prefer the content edge only over matches depending on the SAME anchor.
        if edges[1] > 0 && found.iter().any(|(r, w)| r.start == edges[1] && depends_on(r, w, true)) {
            found.retain(|(r, w)| r.start >= edges[1] || !depends_on(r, w, true));
        }
        if edges[3] < edges[2] && found.iter().any(|(r, w)| r.end == edges[3] && depends_on(r, w, false)) {
            found.retain(|(r, w)| r.end <= edges[3] || !depends_on(r, w, false));
        }
        // The windows overlap: keep the leftmost, longest match of each place.
        found.sort_by_key(|(r, _)| (r.start, std::cmp::Reverse(r.end)));
        let mut end = 0;
        found.retain(|(r, _)| {
            let keep = r.start >= end;
            if keep {
                end = r.end;
            }
            keep
        });
        found.into_iter().map(|(range, _)| range).collect()
    }

    /// The matches of `source` (a line's text, markup included) as UTF-16
    /// `(start, end)` ranges into it; empty when the line does not match.
    /// `view` is scratch space, reused across lines.
    pub fn match_ranges(&self, source: &str, view: &mut LineView) -> Vec<(u32, u32)> {
        view.fill(source, false);
        if !self.is_match(&view.text) {
            return Vec::new();
        }
        view.fill(source, true);
        self.find(&view.text).iter().map(|r| view.source_range(r)).collect()
    }
}

/// Otzaria's `build_line_regex`: the same meta engine and limits as
/// `regex::RegexBuilder`, with bounded search inputs exposed so disabling an
/// anchor cannot let a consuming alternative eat the sentinel.
fn build_line_regex(source: &str) -> Result<LineRegex, String> {
    LineRegex::builder()
        .configure(
            LineRegex::config()
                .match_kind(regex_automata::MatchKind::LeftmostFirst)
                .utf8_empty(true)
                .nfa_size_limit(Some(LINE_REGEX_SIZE_LIMIT))
                .hybrid_cache_capacity(LINE_REGEX_SIZE_LIMIT),
        )
        .syntax(regex_automata::util::syntax::Config::new().case_insensitive(true).utf8(true))
        .build(source)
        .map_err(|e| format!("too complex: {e}"))
}

fn without_word_looks(hir: &Hir) -> Hir {
    match hir.kind() {
        HirKind::Look(look) if !matches!(look, Look::Start | Look::End) => Hir::empty(),
        HirKind::Repetition(rep) => {
            Hir::repetition(Repetition { sub: Box::new(without_word_looks(&rep.sub)), ..rep.clone() })
        }
        HirKind::Capture(capture) => without_word_looks(&capture.sub),
        HirKind::Concat(subs) => Hir::concat(subs.iter().map(without_word_looks).collect()),
        HirKind::Alternation(subs) => Hir::alternation(subs.iter().map(without_word_looks).collect()),
        _ => hir.clone(),
    }
}

/// Otzaria's `WORD_CLASS` (`\w`).
fn word_class() -> &'static regex_syntax::hir::ClassUnicode {
    static W: std::sync::OnceLock<regex_syntax::hir::ClassUnicode> = std::sync::OnceLock::new();
    W.get_or_init(|| match regex_syntax::parse(r"\w").map(Hir::into_kind) {
        Ok(HirKind::Class(Class::Unicode(class))) => class,
        _ => regex_syntax::hir::ClassUnicode::empty(),
    })
}

/// Otzaria's `edge_chars`: whether the first (or last) character of a match
/// can be a word character, and whether a match can be empty there.
fn edge_chars(hir: &Hir, first: bool) -> (bool, bool) {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => (false, true),
        HirKind::Literal(_) => {
            let text = literal_text(hir).unwrap_or_default();
            let c = if first { text.chars().next() } else { text.chars().next_back() };
            (c.is_some_and(is_word_char), false)
        }
        HirKind::Class(Class::Unicode(class)) => {
            let mut word = class.clone();
            word.intersect(word_class());
            (!word.ranges().is_empty(), false)
        }
        HirKind::Class(Class::Bytes(_)) => (true, false),
        HirKind::Repetition(rep) => {
            let (word, empty) = edge_chars(&rep.sub, first);
            (word, empty || rep.min == 0)
        }
        HirKind::Capture(capture) => edge_chars(&capture.sub, first),
        HirKind::Alternation(subs) => subs.iter().fold((false, false), |acc, sub| {
            let (w, e) = edge_chars(sub, first);
            (acc.0 || w, acc.1 || e)
        }),
        HirKind::Concat(subs) => {
            let mut word = false;
            let ordered: Box<dyn Iterator<Item = &Hir>> =
                if first { Box::new(subs.iter()) } else { Box::new(subs.iter().rev()) };
            for sub in ordered {
                let (w, e) = edge_chars(sub, first);
                word |= w;
                if !e {
                    return (word, false);
                }
            }
            (word, true)
        }
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || is_word_mark(c)
}

/// A view char that continues a term in the scanner: a word char, a quote
/// (inside a term or around it) or a transparent char (dropped inside a
/// term, `.` in `a.b`). Every other view char comes from the same source
/// char, or is the space of a separator, so the scanner ends a word there.
fn continues_term(c: char) -> bool {
    crate::chars::is_word_char(c) || is_quote(c) || is_transparent(c)
}

/// True when every char `hir` can match in the view ends a term in the
/// scanner (a separator the pattern names, such as ` `, `,` or `[ ,]`), at
/// least once: the line has a word boundary there in the index too.
fn ends_terms(hir: &Hir) -> bool {
    match hir.kind() {
        HirKind::Literal(_) => literal_text(hir).is_some_and(|t| !t.is_empty() && !t.chars().any(continues_term)),
        HirKind::Class(Class::Unicode(class)) => {
            !class.ranges().is_empty() && class.ranges().iter().all(|r| !(r.start()..=r.end()).any(continues_term))
        }
        HirKind::Repetition(rep) => rep.min >= 1 && ends_terms(&rep.sub),
        HirKind::Capture(capture) => ends_terms(&capture.sub),
        HirKind::Alternation(subs) => subs.iter().all(ends_terms),
        _ => false,
    }
}

/// The fixed words of the top level (Otzaria's `required_clauses`, with the
/// superset fix of Otzaria PR 66): AND-ed clauses whose candidate terms are
/// guaranteed to name every line the pattern matches. A run of literal
/// chars that continue a term is cut into terms by the index's own scanner;
/// a term is anchored at an edge only where the pattern puts a separator
/// ([`ends_terms`]) right there. A pattern edge or a wildcard leaves the edge
/// open: the line may glue more letters there (`x.ab`, `x"ab`, `\u{00B2}ab`).
/// A top-level group whose every branch is a fixed word contributes their OR.
fn required_clauses(elements: &[Hir]) -> Vec<Vec<Fragment>> {
    fn flush(run: &mut String, left: bool, right: bool, clauses: &mut Vec<Vec<Fragment>>) {
        for fragment in fragments(run, left, right) {
            if !clauses.iter().any(|c| c.len() == 1 && c[0] == fragment) {
                clauses.push(vec![fragment]);
            }
        }
        run.clear();
    }
    let mut clauses: Vec<Vec<Fragment>> = Vec::new();
    let mut run = String::new();
    let mut left_bounded = false;
    for element in elements {
        if let Some(text) = literal_text(element) {
            for c in text.chars() {
                if continues_term(c) {
                    run.push(c);
                } else {
                    flush(&mut run, left_bounded, true, &mut clauses);
                    left_bounded = true;
                }
            }
            continue;
        }
        // a line anchor is a scanner boundary too (Otzaria 7.0.0)
        let bounding = ends_terms(element) || matches!(element.kind(), HirKind::Look(Look::Start | Look::End));
        flush(&mut run, left_bounded, bounding, &mut clauses);
        left_bounded = bounding;
        if !bounding {
            if let Some(any) = alternatives(element) {
                clauses.push(any);
            }
        }
    }
    flush(&mut run, left_bounded, false, &mut clauses);
    clauses
}

/// Otzaria's `alternatives`: the literal branches of a group required at
/// least once, each its longest term, open on both edges.
fn alternatives(hir: &Hir) -> Option<Vec<Fragment>> {
    match hir.kind() {
        HirKind::Repetition(rep) if rep.min >= 1 => alternatives(&rep.sub),
        HirKind::Capture(capture) => alternatives(&capture.sub),
        HirKind::Alternation(subs) => subs
            .iter()
            .map(|sub| {
                let text: String = literal_text(sub)?.chars().map(|c| if continues_term(c) { c } else { ' ' }).collect();
                fragments(&text, false, false).into_iter().max_by_key(|f| f.text.chars().count())
            })
            .collect(),
        _ => None,
    }
}

/// The terms the index makes of `run` (the scanner itself: quotes, `''`,
/// transparent chars, case and marks as in the index), quotes removed (the
/// walk reads quotes zero-width). `left` / `right`: the run is bounded there
/// by a separator; inside the run the scanner's own cuts bound the terms. A
/// term of fewer than two letters narrows nothing; one too long to be
/// indexed is no term at all (its lines come from the long-word flag).
fn fragments(run: &str, left: bool, right: bool) -> Vec<Fragment> {
    let mut words: Vec<(String, bool)> = Vec::new();
    crate::scanner::scan_with(run, false, |word, span| {
        words.push((word.chars().filter(|c| !is_quote(*c)).collect(), is_indexable_len(span.term_bytes)));
    });
    let last = words.len().saturating_sub(1);
    words
        .into_iter()
        .enumerate()
        .filter(|(_, (text, indexed))| *indexed && text.chars().count() >= 2)
        .map(|(i, (text, _))| Fragment { text, start: left || i > 0, end: right || i < last })
        .collect()
}

/// What the view deletes: the marks the scanner keeps inside a word and
/// strips from its term (nikud, cantillation, combining accents). Maqaf,
/// paseq, sof pasuq and nun hafukha end a word in the scanner, so the view
/// turns them into a space (`cleanLineForSearch` deletes sof pasuq, which
/// would glue two words the index keeps apart).
fn is_dropped_mark(c: char) -> bool {
    is_word_mark(c)
}

/// Otzaria's `is_breaking_tag_name` on the text between `<` and `>`.
pub(crate) fn is_breaking_tag_name(body: &str) -> bool {
    let body = body.strip_prefix('/').unwrap_or(body);
    let len = body.bytes().take_while(u8::is_ascii_alphanumeric).count();
    (1..=10).contains(&len) && BREAKING_TAG_NAMES.iter().any(|name| name.eq_ignore_ascii_case(&body[..len]))
}

/// A line as a line pattern sees it (Otzaria's `LineView`, with the word
/// breaks of the index's scanner, so the candidate terms always name every
/// matching line): markup removed (a tag that ends a word in the scanner and
/// every entity become a space, other tags vanish), nikud and cantillation
/// removed, maqaf, paseq, sof pasuq, nun hafukha and `|` as a space,
/// whitespace collapsed and trimmed, quotes as ASCII, presentation forms
/// decomposed, invisible characters dropped, lowercase. With a map, every
/// byte of `text` knows the UTF-16 range of the source it came from.
#[derive(Default, Debug)]
pub struct LineView {
    pub text: String,
    starts: Vec<u32>,
    ends: Vec<u32>,
}

impl LineView {
    pub fn of(source: &str, map: bool) -> Self {
        let mut view = LineView::default();
        view.fill(source, map);
        view
    }

    pub fn fill(&mut self, source: &str, map: bool) {
        self.text.clear();
        self.starts.clear();
        self.ends.clear();
        let bytes = source.as_bytes();
        let mut pending: Option<(u32, u32)> = None;
        let mut has_tag_end = true;
        let mut sup_transparent = false;
        let mut byte = 0usize;
        let mut utf16 = 0u32;
        let units = |text: &str| if map { text.encode_utf16().count() as u32 } else { 0 };
        while byte < bytes.len() {
            let b = bytes[byte];
            if b == b'<' && has_tag_end {
                let rest = &source[byte..];
                if let Some(close) = rest.find('>') {
                    let end16 = utf16 + units(&rest[..=close]);
                    if tag_breaks_word(source, byte, byte + close, &mut sup_transparent) {
                        pending = Some(pending.map_or((utf16, end16), |(s, _)| (s, end16)));
                    }
                    byte += close + 1;
                    utf16 = end16;
                    continue;
                }
                has_tag_end = false;
            } else if b == b'&' {
                // As the scanner: an entity (a name of at most 10 units, then
                // `;`) or a bare `&` is a separator, here a space.
                let (mut end, mut unused) = (byte, 0usize);
                skip_entity(bytes, &mut end, &mut unused);
                let end16 = utf16 + units(&source[byte..=end]);
                pending = Some(pending.map_or((utf16, end16), |(s, _)| (s, end16)));
                byte = end + 1;
                utf16 = end16;
                continue;
            }
            if b < 0x80 {
                byte += 1;
                if matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C | b'|') {
                    pending = Some(pending.map_or((utf16, utf16 + 1), |(s, _)| (s, utf16 + 1)));
                } else {
                    self.flush_space(&mut pending, map);
                    self.push(char::from(b.to_ascii_lowercase()), utf16, utf16 + 1, map);
                }
                utf16 += 1;
                continue;
            }
            let c = source[byte..].chars().next().expect("not at the end");
            byte += c.len_utf8();
            let next16 = utf16 + c.len_utf16() as u32;
            if ('\u{05D0}'..='\u{05EA}').contains(&c) {
                self.flush_space(&mut pending, map);
                self.push(c, utf16, next16, map);
            } else if is_dropped_mark(c) {
                // A mark is painted with the letter it sits on.
                if map && pending.is_none() {
                    if let Some(last) = self.text.chars().next_back() {
                        let n = self.ends.len();
                        self.ends[n - last.len_utf8()..].fill(next16);
                    }
                }
            } else if c.is_whitespace() || matches!(c, '\u{05BE}' | '\u{05C0}' | '\u{05C3}' | '\u{05C6}') {
                pending = Some(pending.map_or((utf16, next16), |(s, _)| (s, next16)));
            } else if !is_invisible(c) {
                self.flush_space(&mut pending, map);
                match c {
                    '\u{05F3}' | '\u{2018}' | '\u{2019}' => self.push('\'', utf16, next16, map),
                    '\u{05F4}' | '\u{201C}' | '\u{201D}' => self.push('"', utf16, next16, map),
                    _ => match fold_presentation_form(c) {
                        Some(folded) => {
                            for f in folded.chars().filter(|f| !is_word_mark(*f)) {
                                self.push(f, utf16, next16, map);
                            }
                        }
                        None if c.is_uppercase() => {
                            for l in c.to_lowercase() {
                                self.push(l, utf16, next16, map);
                            }
                        }
                        None => self.push(c, utf16, next16, map),
                    },
                }
            }
            utf16 = next16;
        }
    }

    /// Emits a pending whitespace run as one space, unless the view is still empty.
    fn flush_space(&mut self, pending: &mut Option<(u32, u32)>, map: bool) {
        if let Some((s, e)) = pending.take() {
            if !self.text.is_empty() {
                self.push(' ', s, e, map);
            }
        }
    }

    fn push(&mut self, c: char, start: u32, end: u32, map: bool) {
        self.text.push(c);
        if map {
            self.starts.extend(std::iter::repeat_n(start, c.len_utf8()));
            self.ends.extend(std::iter::repeat_n(end, c.len_utf8()));
        }
    }

    /// The UTF-16 source range of the view's byte range (non-empty; the
    /// view must have been filled with a map).
    pub fn source_range(&self, range: &std::ops::Range<usize>) -> (u32, u32) {
        (self.starts[range.start], self.ends[range.end - 1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_regex::{classify, parse_line};

    #[test]
    fn plan_clauses_and_matching() {
        // Hebrew letters: Latin ones fold to [Aa] classes under the
        // case-insensitive parse (as in Otzaria), so they are no fragments.
        const AB: &str = "\u{05d0}\u{05d1}";
        const CD: &str = "\u{05d2}\u{05d3}";
        // "ab .* cd": two fixed words, each bounded by its space; the
        // pattern edges stay open (the line may glue `x.ab` into one term)
        let plan = parse_line(&format!("/{AB} .* {CD}/")).unwrap();
        assert_eq!(
            plan.clauses,
            vec![
                vec![Fragment { text: AB.into(), start: false, end: true }],
                vec![Fragment { text: CD.into(), start: true, end: false }]
            ]
        );
        let mut view = LineView::default();
        assert_eq!(plan.match_ranges(&format!("x {AB} y z {CD} w"), &mut view), vec![(2, 11)]);
        assert!(plan.match_ranges(&format!("x{AB} y {CD}"), &mut view).is_empty()); // implied word boundary
        // a fragment touching a wildcard is only contained in a term
        let plan = parse_line(&format!("/{AB}\\w+[ ,]{CD}/")).unwrap();
        assert_eq!(plan.clauses[0], vec![Fragment { text: AB.into(), start: false, end: false }]);
        // a quote inside a run is the scanner's: the term without its quotes;
        // `[ ,]ab"cd[ ,]` is bounded on both sides
        let plan = parse_line(&format!("/[ ,]{AB}\"{CD}[ ,]/")).unwrap();
        assert_eq!(plan.clauses, vec![vec![Fragment { text: format!("{AB}{CD}"), start: true, end: true }]]);
        // a transparent `.` continues the term (`ab.cd` is one term)
        let plan = parse_line(&format!("/[ ,]{AB}\\.{CD}[ ,]/")).unwrap();
        assert_eq!(plan.clauses, vec![vec![Fragment { text: format!("{AB}{CD}"), start: true, end: true }]]);
        assert_eq!(classify(&format!("/{AB} .* {CD}/")), "line");
    }

    #[test]
    fn view_folds_markup_and_maps_utf16() {
        // a breaking tag is a space, an inline tag vanishes, marks are dropped
        let src = "A <b>b</b><br>c\u{05B8}d";
        let view = LineView::of(src, true);
        assert_eq!(view.text, "a b cd");
        let plan = parse_line("/b[ ,]cd/").unwrap();
        let mut v = LineView::default();
        let ranges = plan.match_ranges(src, &mut v);
        assert_eq!(ranges.len(), 1);
        let (s, e) = ranges[0];
        let units: Vec<u16> = src.encode_utf16().collect();
        assert_eq!(String::from_utf16(&units[s as usize..e as usize]).unwrap(), "b</b><br>c\u{05B8}d");
    }

    // Otzaria 7.0.0 line anchors (otzaria_search_engine 5babbce,
    // `user_regex_tests.rs`: an_edge_line_anchor_makes_a_line_pattern,
    // line_anchors_hold_at_the_line_edges_and_past_enumerators,
    // a_bracket_enumerator_glued_to_the_word_is_part_of_it,
    // an_end_anchor_preserves_an_independent_start_match,
    // a_start_anchor_preserves_an_independent_end_match,
    // a_consuming_alternative_keeps_the_other_anchored_match,
    // anchored_fragments_keep_their_guaranteed_token_boundary,
    // suppressing_an_anchor_keeps_an_independent_path_to_the_same_match),
    // the same words as escapes.
    const SEFER: &str = "\u{05e1}\u{05e4}\u{05e8}";
    const SHEMA: &str = "\u{05e9}\u{05de}\u{05e2}";
    const YISRAEL: &str = "\u{05d9}\u{05e9}\u{05e8}\u{05d0}\u{05dc}";
    const VY: &str = "\u{05d5}\u{05d9}\u{05d0}\u{05de}\u{05e8}";
    const EL: &str = "\u{05d0}\u{05dc}\u{05d4}\u{05d9}\u{05dd}";
    const G: &str = "\u{05d2}";
    const P: &str = "\u{05e4}";
    const A: &str = "\u{05d0}";

    fn matches(raw: &str, line: &str) -> Vec<String> {
        let units: Vec<u16> = line.encode_utf16().collect();
        parse_line(raw)
            .unwrap()
            .match_ranges(line, &mut LineView::default())
            .iter()
            .map(|&(s, e)| String::from_utf16(&units[s as usize..e as usize]).unwrap())
            .collect()
    }

    #[test]
    fn an_edge_line_anchor_makes_a_line_pattern() {
        use crate::user_regex::{parse, UserRegexError};
        for raw in [
            format!("/^{SEFER}/"),
            format!("/{SEFER}$/"),
            format!("/^{SEFER}$/"),
            format!("/^{SHEMA} {YISRAEL}/"),
            format!("/^{A}.*/"),
            format!(r"/^\b{SEFER}\b$/"),
            format!("/(?m)^{SEFER}/"),
            format!("/(?mR)^{SEFER}$/"),
        ] {
            assert_eq!(classify(&raw), "line", "{}", raw.escape_unicode());
        }
        // word boundaries and spaces at the edges are still dropped
        assert_eq!(classify(&format!(r"/\b{SEFER}\b/")), "word");
        assert_eq!(classify(&format!(r"/ \b{SHEMA} {YISRAEL}\b /")), "sequence");
        for raw in ["/^$/", "/^/", "/$/", "/^ $/", r"/^\b$/", "/(?m)^$/"] {
            assert_eq!(classify(raw), "invalid", "{raw}");
        }
        // an anchor that can never hold where it stands makes the pattern invalid
        for raw in [
            format!("/${A}/"),
            format!("/{A}^/"),
            format!("/^${A}/"),
            format!("/{A}^$/"),
            format!("/(?m)${A}/"),
            format!(r"/ \b${A}/"),
            format!(r"/{A}^\b /"),
        ] {
            assert_eq!(
                parse(&raw).err(),
                Some(UserRegexError::Invalid("anchor on the wrong edge".into())),
                "{}",
                raw.escape_unicode()
            );
        }
        // an anchor bounds the word next to it, which then narrows as a whole term
        let frag = |text: &str, start, end| vec![vec![Fragment { text: text.into(), start, end }]];
        assert_eq!(parse_line(&format!("/^{SHEMA} .*/")).unwrap().clauses, frag(SHEMA, true, true));
        assert_eq!(parse_line(&format!("/.* {YISRAEL}$/")).unwrap().clauses, frag(YISRAEL, true, true));
        assert_eq!(parse_line(&format!("/^{SEFER}$/")).unwrap().clauses, frag(SEFER, true, true));
        assert_eq!(parse_line(&format!("/^{SHEMA}.*/")).unwrap().clauses, frag(SHEMA, true, false));
        assert_eq!(parse_line(&format!("/{SEFER}$/")).unwrap().clauses, frag(SEFER, false, true));
        assert!(parse_line(&format!("/^{A}.*/")).unwrap().clauses.is_empty());
    }

    #[test]
    fn line_anchors_hold_at_the_line_edges_and_past_enumerators() {
        let start = format!("/^{VY}/");
        let start = start.as_str();
        assert_eq!(matches(start, &format!("{VY} {EL}")), [VY]);
        assert_eq!(matches(start, &format!("({G}) {VY} {EL}")), [VY]);
        assert_eq!(matches(start, &format!("[\u{05d9}{A}] {VY} {EL}")), [VY]);
        assert_eq!(matches(start, &format!("{{{P}}} {VY}")), [VY]);
        let vocalized = "\u{05d5}\u{05b7}\u{05d9}\u{05b9}\u{05bc}\u{05d0}\u{05de}\u{05b6}\u{05e8}";
        assert_eq!(matches(start, &format!("<b>({G})</b> {vocalized} {EL}")), [vocalized]);
        assert!(matches(start, &format!("{A}\u{05d6} {VY} {EL}")).is_empty());
        assert!(matches(start, &format!("{VY}\u{05d5} {EL}")).is_empty());
        assert!(matches(start, &format!("(\u{05e9}\u{05dd} \u{05d4}{SEFER}) {VY}")).is_empty());
        assert_eq!(matches(&format!("/(?m)^{VY}/"), &format!("({G}) {VY}")), [VY]);
        assert_eq!(matches(&format!(r"/^\({G}\) {VY}/"), &format!("({G}) {VY} {EL}")), [format!("({G}) {VY}")]);
        // one match per anchor: past the enumerator when the pattern holds there
        assert_eq!(matches("/^.*/", &format!("({G}) {VY}")), [VY]);
        assert_eq!(matches(r"/^\S+/", &format!("({G}) {VY} {EL}")), [VY]);
        assert_eq!(matches(r"/\S+$/", &format!("{VY} {EL} {{{P}}}")), [EL]);
        assert_eq!(matches(r"/^\S+$/", &format!("({G}) {VY} {{{P}}}")), [VY]);
        // a match that needs no anchor is kept beside it
        let line = format!("({G}) {VY} {EL}");
        assert_eq!(matches(&format!(r"/^\({G}\)|{VY}/"), &line), [format!("({G})"), VY.into()]);
        assert_eq!(matches(&format!(r"/^\({G}\) {VY}|{VY}/"), &line), [format!("({G}) {VY}")]);
        assert_eq!(matches(&format!(r"/^{VY}|\({G}\)/"), &line), [format!("({G})"), VY.into()]);
        let line = format!("{VY} {EL} {{{P}}}");
        assert_eq!(matches(&format!(r"/{EL}$|\{{{P}\}}/"), &line), [EL.into(), format!("{{{P}}}")]);
        assert_eq!(matches(&format!("/^{VY} .*/"), &format!("({G}) {VY} {EL}")), [format!("{VY} {EL}")]);

        let end = format!("/{EL}$/");
        let end = end.as_str();
        assert_eq!(matches(end, &format!("{VY} {EL}")), [EL]);
        assert_eq!(matches(end, &format!("{VY} {EL} {{{P}}}")), [EL]);
        assert_eq!(matches(end, &format!("{VY} {EL} ({A}):")), [EL]);
        assert!(matches(end, &format!("{EL} {VY}")).is_empty());
        assert!(matches(end, &format!("{VY} {EL}:")).is_empty());
        let vayehi_ken = "\u{05d5}\u{05d9}\u{05d4}\u{05d9} \u{05db}\u{05df}";
        assert_eq!(matches(&format!("/^{vayehi_ken}$/"), &format!("(\u{05d6}) {vayehi_ken} {{{P}}}")), [vayehi_ken]);
        assert_eq!(matches(&format!("/^{SEFER}$/"), SEFER), [SEFER]);
        assert!(matches(&format!("/^{SEFER}$/"), &format!("{SEFER} \u{05ea}\u{05d5}\u{05e8}\u{05d4}")).is_empty());
        // a bracket enumerator glued to the word is part of it
        assert!(matches(start, &format!("[{A}]{VY} x")).is_empty());
        assert_eq!(matches(start, &format!("[{A}] {VY} x")), [VY]);
        assert!(matches(end, &format!("x {EL}[{A}]")).is_empty());
        assert_eq!(matches(end, &format!("x {EL} [{A}]")), [EL]);
    }

    #[test]
    fn an_anchor_keeps_independent_matches() {
        // an end anchor preserves an independent start match, and the reverse
        assert_eq!(matches(&format!(r"/^\({G}\)|{EL}$/"), &format!("({G}) {EL}")), [format!("({G})"), EL.into()]);
        assert_eq!(matches(&format!(r"/^{VY}|\{{{P}\}}$/"), &format!("{VY} {{{P}}}")), [VY.into(), format!("{{{P}}}")]);
        // a consuming alternative keeps the other anchored match
        assert_eq!(
            matches(&format!(r"/\{{{P}\}}$|{EL}\W?/"), &format!("{VY} {EL} {{{P}}}")),
            [format!("{EL} "), format!("{{{P}}}")]
        );
        // suppressing an anchor keeps an independent path to the same match
        assert_eq!(
            matches(&format!(r"/^\({G}\)|\({G}\)|^{VY}/"), &format!("({G}) {VY}")),
            [format!("({G})"), VY.into()]
        );
        assert_eq!(
            matches(&format!(r"/{EL}$|\{{{P}\}}$|\{{{P}\}}/"), &format!("{VY} {EL} {{{P}}}")),
            [EL.into(), format!("{{{P}}}")]
        );
    }
}
