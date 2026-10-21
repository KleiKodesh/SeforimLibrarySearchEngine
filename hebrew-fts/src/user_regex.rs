//! User regex queries `/.../`: Otzaria's `user_regex.rs` (its `parse`,
//! `fold_pattern` and the `Word` / `Sequence` kinds), as FST walks.
//!
//! - Detection (Otzaria's Dart `UserRegexQuery._isWrapped`): the trimmed
//!   query is longer than 2 chars, starts and ends with `/`, and has no inner
//!   slash next to a space (`/a/ /b/` is two words, not a regex).
//! - The inner pattern is folded the way the index folds text: Hebrew marks
//!   (U+0591..U+05C7) dropped, maqaf / paseq a space, geresh and typographic
//!   single quotes `'`, gershayim and typographic double quotes `"`. At most
//!   300 chars (Otzaria's `MAX_PATTERN_CHARS`).
//! - Parsed case-insensitively in the Rust `regex` dialect (classes, `\w`,
//!   `{m,n}`, alternation, Unicode classes; no lookaround, no
//!   backreferences). Anchors, empty items and whitespace at the edges are
//!   dropped; a term pattern is whole-term (implicitly `\A...\z`).
//! - The top level is split on whitespace separators into words. One word =
//!   Otzaria's `Word` (a term set), several = `Sequence` (an adjacent,
//!   ordered phrase). Each word compiles as Otzaria's `prepare_term_part`:
//!   classes narrowed to its term alphabet, lazy made greedy, long bounded
//!   repeats unbounded, an anchored dense DFA (its `term_automaton`) and a
//!   verifier when relaxing let in more terms. The DFA is walked over the
//!   term dictionary, quote transparent (Otzaria's `QuoteTransparent`),
//!   capped shortest first like every other walk here, with the
//!   matched-terms sink.
//! - Otzaria's third kind, `Line` (a pattern crossing spaces explicitly,
//!   inner anchors / `\b`, or a word made only of wildcards in a sequence),
//!   is matched on each line's TEXT. It is reported as
//!   [`UserRegexError::LineKind`]: see the README's open items.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use regex::{Regex, RegexBuilder};
use regex_automata::dfa::{dense, Automaton as _, StartKind};
use regex_automata::nfa::thompson::{self, WhichCaptures};
use regex_automata::util::primitives::StateID;
use regex_automata::util::start;
use regex_automata::{Anchored, MatchKind};
use regex_syntax::hir::{Class, ClassUnicode, ClassUnicodeRange, Hir, HirKind, Repetition};
use tantivy::query::{EnableScoring, MatchedTermsSink, Query, SpanTermSource, Weight};
use tantivy::schema::Field;
use tantivy_fst::Automaton;

use crate::quote_free::QuoteFree;
use crate::walk::DEFAULT_MAX_EXPANDED_TERMS;

/// Otzaria's `MAX_BOUNDED_REPEAT`: a bounded repetition above it is
/// unbounded for the walk (the verifier keeps the bound).
const MAX_BOUNDED_REPEAT: u32 = 7;
/// Otzaria's `TERM_COST_LIMIT` / `REGEX_COST_LIMIT`.
const TERM_COST_LIMIT: u64 = 2_000;
const REGEX_COST_LIMIT: u64 = 20_000;
/// Otzaria's `LINE_REGEX_SIZE_LIMIT` (the verifier's limits).
const VERIFY_SIZE_LIMIT: usize = 8 << 20;
/// Otzaria's `term_automaton` limits.
const NFA_SIZE_LIMIT: usize = 10 << 20;
const DFA_SIZE_LIMIT: usize = 32 << 20;
const DETERMINIZE_SIZE_LIMIT: usize = 64 << 20;

/// Otzaria's `MAX_PATTERN_CHARS`: the folded inner pattern's limit.
pub const MAX_PATTERN_CHARS: usize = 300;

/// Why a wrapped query is not a term regex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserRegexError {
    /// Longer than [`MAX_PATTERN_CHARS`], empty, a syntax error, only
    /// anchors / spaces, or too complex to compile (Otzaria: kind `Invalid`;
    /// the app then searches the query as plain text).
    Invalid(String),
    /// Otzaria's `Line` kind (`CrossSpace`): matched on the line text, not
    /// on dictionary terms. Not supported yet.
    LineKind,
}

impl std::fmt::Display for UserRegexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UserRegexError::Invalid(r) => write!(f, "invalid user regex: {r}"),
            UserRegexError::LineKind => write!(
                f,
                "user regex crosses spaces or uses inner anchors (Otzaria's line kind): not supported yet"
            ),
        }
    }
}

/// True when `query` is a wrapped regex `/.../` (Otzaria's `_isWrapped`):
/// trimmed, longer than 2 (UTF-16) chars, starts and ends with `/`, and no
/// slash next to a space between the outer slashes.
pub fn is_wrapped(query: &str) -> bool {
    let q = query.trim();
    if q.encode_utf16().count() <= 2 || !q.starts_with('/') || !q.ends_with('/') {
        return false;
    }
    let inner: Vec<char> = q[1..q.len() - 1].chars().collect();
    !inner
        .windows(2)
        .any(|w| (w[0].is_whitespace() && w[1] == '/') || (w[0] == '/' && w[1].is_whitespace()))
}

/// What a query is, as Otzaria's `parse_user_regex` reports it: `"none"`
/// (not wrapped in `/.../`), `"invalid"` (searched as plain text),
/// `"word"`, `"sequence"`, or `"line"` (Otzaria's line kind, not supported
/// yet).
pub fn classify(query: &str) -> &'static str {
    if !is_wrapped(query) {
        return "none";
    }
    match parse(query) {
        Ok(re) if re.words.len() == 1 => "word",
        Ok(_) => "sequence",
        Err(UserRegexError::LineKind) => "line",
        Err(UserRegexError::Invalid(_)) => "invalid",
    }
}

/// Folds the pattern as the index folds text (Otzaria's `fold_pattern`):
/// marks dropped (the U+0591..U+05C7 block and every word mark), maqaf and
/// paseq a space, Hebrew and typographic quotes as ASCII, presentation forms
/// decomposed (their marks dropped), invisible chars dropped.
pub fn fold_pattern(raw: &str) -> String {
    use crate::chars::{fold_presentation_form, is_invisible, is_word_mark};
    let dropped_mark =
        |c: char| (matches!(c, '\u{0591}'..='\u{05C7}') && !matches!(c, '\u{05BE}' | '\u{05C0}')) || is_word_mark(c);
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if dropped_mark(c) || is_invisible(c) {
            continue;
        }
        match c {
            '\u{05F3}' | '\u{2018}' | '\u{2019}' => out.push('\''),
            '\u{05F4}' | '\u{201C}' | '\u{201D}' => out.push('"'),
            '\u{05BE}' | '\u{05C0}' => out.push(' '),
            _ => match fold_presentation_form(c) {
                Some(folded) => out.extend(folded.chars().filter(|c| !is_word_mark(*c))),
                None => out.push(c),
            },
        }
    }
    out
}

/// A parsed term regex: one whole-term pattern per word, in order.
#[derive(Debug, Clone)]
pub struct UserRegex {
    /// Each word's exact pattern (printed from the parsed, case-folded HIR);
    /// [`HebrewRegexQuery::new`] takes one.
    pub words: Vec<String>,
}

/// Otzaria's `strip_captures` (its `relax` makes lazy quantifiers greedy
/// for the walk: a whole term has one match either way).
fn strip_captures(hir: Hir) -> Hir {
    match hir.into_kind() {
        HirKind::Capture(capture) => strip_captures(*capture.sub),
        HirKind::Concat(subs) => Hir::concat(subs.into_iter().map(strip_captures).collect()),
        HirKind::Alternation(subs) => Hir::alternation(subs.into_iter().map(strip_captures).collect()),
        HirKind::Repetition(rep) => Hir::repetition(Repetition {
            sub: Box::new(strip_captures(*rep.sub)),
            ..rep
        }),
        kind => rebuild(kind),
    }
}

fn rebuild(kind: HirKind) -> Hir {
    match kind {
        HirKind::Empty => Hir::empty(),
        HirKind::Literal(lit) => Hir::literal(lit.0),
        HirKind::Class(class) => Hir::class(class),
        HirKind::Look(look) => Hir::look(look),
        HirKind::Repetition(rep) => Hir::repetition(rep),
        HirKind::Capture(capture) => Hir::capture(capture),
        HirKind::Concat(subs) => Hir::concat(subs),
        HirKind::Alternation(subs) => Hir::alternation(subs),
    }
}

/// Otzaria's `top_elements`: the top-level concatenation, every literal
/// split into its characters (a merged literal may hold a space).
fn top_elements(hir: Hir) -> Vec<Hir> {
    let subs = match hir.into_kind() {
        HirKind::Concat(subs) => subs,
        kind => vec![rebuild(kind)],
    };
    let mut out = Vec::with_capacity(subs.len());
    for sub in subs {
        match sub.kind() {
            HirKind::Literal(lit) => match std::str::from_utf8(&lit.0) {
                Ok(text) => {
                    for c in text.chars() {
                        let mut buf = [0u8; 4];
                        out.push(Hir::literal(c.encode_utf8(&mut buf).as_bytes().to_vec()));
                    }
                }
                Err(_) => out.push(sub),
            },
            _ => out.push(sub),
        }
    }
    out
}

fn literal_text(hir: &Hir) -> Option<&str> {
    match hir.kind() {
        HirKind::Literal(lit) => std::str::from_utf8(&lit.0).ok(),
        _ => None,
    }
}

/// Otzaria's `is_separator`: whitespace and nothing else, at least once.
fn is_separator(hir: &Hir) -> bool {
    match hir.kind() {
        HirKind::Literal(_) => literal_text(hir).is_some_and(|t| t.chars().all(char::is_whitespace)),
        HirKind::Class(Class::Unicode(class)) => {
            !class.ranges().is_empty()
                && class.ranges().iter().all(|r| (r.start()..=r.end()).all(char::is_whitespace))
        }
        HirKind::Repetition(rep) => rep.min >= 1 && is_separator(&rep.sub),
        _ => false,
    }
}

fn class_contains(class: &ClassUnicode, c: char) -> bool {
    class.ranges().iter().any(|r| r.start() <= c && c <= r.end())
}

/// Otzaria's `has_explicit_whitespace`: a literal space, or a class with a
/// space and no letter (`[ ,]`, `\W`); `.` and `[^x]` have letters too.
fn has_explicit_whitespace(hir: &Hir) -> bool {
    match hir.kind() {
        HirKind::Literal(_) => literal_text(hir).is_some_and(|t| t.chars().any(char::is_whitespace)),
        HirKind::Class(Class::Unicode(class)) => {
            class_contains(class, ' ') && !class_contains(class, '\u{05D0}') && !class_contains(class, 'a')
        }
        HirKind::Class(Class::Bytes(_)) | HirKind::Empty | HirKind::Look(_) => false,
        HirKind::Repetition(rep) => has_explicit_whitespace(&rep.sub),
        HirKind::Capture(capture) => has_explicit_whitespace(&capture.sub),
        HirKind::Concat(subs) | HirKind::Alternation(subs) => subs.iter().any(has_explicit_whitespace),
    }
}

/// Otzaria's `is_wildcard_word`: a word that names no letter of its own
/// (`.*`, `\S+`).
fn is_wildcard_word(hir: &Hir) -> bool {
    fn literal_free(hir: &Hir) -> bool {
        match hir.kind() {
            HirKind::Literal(_) => false,
            HirKind::Class(Class::Unicode(class)) => class_contains(class, '\u{05D0}'),
            HirKind::Class(Class::Bytes(_)) => true,
            HirKind::Empty | HirKind::Look(_) => true,
            HirKind::Repetition(rep) => literal_free(&rep.sub),
            HirKind::Capture(capture) => literal_free(&capture.sub),
            HirKind::Concat(subs) | HirKind::Alternation(subs) => subs.iter().all(literal_free),
        }
    }
    literal_free(hir)
}

/// Parses a wrapped query (Otzaria's `parse`) into a term regex.
pub fn parse(query: &str) -> Result<UserRegex, UserRegexError> {
    let trimmed = query.trim();
    let inner = trimmed
        .strip_prefix('/')
        .and_then(|rest| rest.strip_suffix('/'))
        .ok_or_else(|| UserRegexError::Invalid("not wrapped in /.../".into()))?;
    if inner.chars().count() > MAX_PATTERN_CHARS {
        return Err(UserRegexError::Invalid(format!("longer than {MAX_PATTERN_CHARS} characters")));
    }
    let folded = fold_pattern(inner);
    if folded.trim().is_empty() {
        return Err(UserRegexError::Invalid("empty pattern".into()));
    }
    let hir = regex_syntax::ParserBuilder::new()
        .case_insensitive(true)
        .unicode(true)
        .utf8(true)
        .nest_limit(40)
        .build()
        .parse(&folded)
        .map_err(|e| match e {
            regex_syntax::Error::Parse(e) => UserRegexError::Invalid(format!("syntax: {}", e.kind())),
            regex_syntax::Error::Translate(e) => UserRegexError::Invalid(format!("syntax: {}", e.kind())),
            other => UserRegexError::Invalid(format!("syntax: {other}")),
        })?;
    let mut elements = top_elements(strip_captures(hir));
    loop {
        let edge = |e: &Hir| matches!(e.kind(), HirKind::Look(_) | HirKind::Empty) || is_separator(e);
        if elements.first().is_some_and(edge) {
            elements.remove(0);
        } else if elements.last().is_some_and(edge) {
            elements.pop();
        } else {
            break;
        }
    }
    if elements.is_empty() {
        return Err(UserRegexError::Invalid("matches only anchors or spaces".into()));
    }
    let inner_looks = elements.iter().any(|e| !e.properties().look_set().is_empty());
    let mut parts: Vec<Vec<Hir>> = vec![Vec::new()];
    for element in &elements {
        if is_separator(element) {
            if parts.last().is_some_and(|p| !p.is_empty()) {
                parts.push(Vec::new());
            }
        } else {
            parts.last_mut().expect("one part at least").push(element.clone());
        }
    }
    let parts: Vec<Hir> = parts.into_iter().map(Hir::concat).collect();
    let spans_space = inner_looks
        || parts.iter().any(has_explicit_whitespace)
        || (parts.len() > 1 && parts.iter().any(is_wildcard_word));
    if spans_space {
        return Err(UserRegexError::LineKind);
    }
    let mut words = Vec::with_capacity(parts.len());
    for part in &parts {
        let pattern = part.to_string();
        term_part(&pattern)?;
        words.push(pattern);
    }
    Ok(UserRegex { words })
}

/// Otzaria's `TERM_ALPHABET`: letters, digits and quotes that index terms
/// are made of. Classes are narrowed to it for the walk.
fn term_alphabet() -> &'static ClassUnicode {
    static A: OnceLock<ClassUnicode> = OnceLock::new();
    A.get_or_init(|| {
        ClassUnicode::new([
            ClassUnicodeRange::new('"', '"'),
            ClassUnicodeRange::new('\'', '\''),
            ClassUnicodeRange::new('0', '9'),
            ClassUnicodeRange::new('a', 'z'),
            ClassUnicodeRange::new('\u{05D0}', '\u{05EA}'),
            ClassUnicodeRange::new('\u{05F0}', '\u{05F2}'),
        ])
    })
}

/// Otzaria's `cost`: roughly the automaton states compiling `hir` takes.
fn cost(hir: &Hir) -> u64 {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => 1,
        HirKind::Literal(lit) => lit.0.len() as u64,
        HirKind::Class(Class::Unicode(class)) => 1 + class.ranges().len() as u64,
        HirKind::Class(Class::Bytes(class)) => 1 + class.ranges().len() as u64,
        HirKind::Repetition(rep) => {
            let copies = u64::from(rep.max.unwrap_or(rep.min).max(rep.min)).max(1) + 1;
            cost(&rep.sub).saturating_mul(copies)
        }
        HirKind::Capture(capture) => cost(&capture.sub),
        HirKind::Concat(subs) | HirKind::Alternation(subs) => {
            subs.iter().fold(0u64, |acc, sub| acc.saturating_add(cost(sub)))
        }
    }
}

fn has_bounded_repetition(hir: &Hir) -> bool {
    match hir.kind() {
        HirKind::Repetition(rep) => rep.max.is_some_and(|max| max > 1) || has_bounded_repetition(&rep.sub),
        HirKind::Capture(capture) => has_bounded_repetition(&capture.sub),
        HirKind::Concat(subs) | HirKind::Alternation(subs) => subs.iter().any(has_bounded_repetition),
        _ => false,
    }
}

/// Otzaria's `narrow`: every class narrowed to the term alphabet.
fn narrow(hir: &Hir) -> Hir {
    match hir.kind() {
        HirKind::Class(Class::Unicode(class)) => {
            let mut narrowed = class.clone();
            narrowed.intersect(term_alphabet());
            Hir::class(Class::Unicode(narrowed))
        }
        HirKind::Repetition(rep) => Hir::repetition(Repetition { sub: Box::new(narrow(&rep.sub)), ..rep.clone() }),
        HirKind::Capture(capture) => narrow(&capture.sub),
        HirKind::Concat(subs) => Hir::concat(subs.iter().map(narrow).collect()),
        HirKind::Alternation(subs) => Hir::alternation(subs.iter().map(narrow).collect()),
        _ => hir.clone(),
    }
}

/// Otzaria's `relax`: classes narrowed, lazy made greedy, long (or, when
/// `aggressive`, every) bounded repetitions unbounded. The flag says the
/// result accepts terms the pattern does not (the verifier then decides).
fn relax(hir: &Hir, aggressive: bool) -> (Hir, bool) {
    match hir.kind() {
        HirKind::Class(Class::Unicode(class)) => {
            let mut narrowed = class.clone();
            narrowed.intersect(term_alphabet());
            (Hir::class(Class::Unicode(narrowed)), false)
        }
        HirKind::Repetition(rep) => {
            let (sub, changed) = relax(&rep.sub, aggressive);
            let limit = if aggressive { 1 } else { MAX_BOUNDED_REPEAT };
            // A bounded count of bounded counts multiplies the DFA's states.
            let nested = rep.max.is_some_and(|max| max > 1) && has_bounded_repetition(&sub);
            let long = nested || rep.min > limit || rep.max.is_some_and(|max| max > limit);
            let (min, max) = if long { (rep.min.min(limit), None) } else { (rep.min, rep.max) };
            (Hir::repetition(Repetition { min, max, greedy: true, sub: Box::new(sub) }), changed || long)
        }
        HirKind::Capture(capture) => relax(&capture.sub, aggressive),
        HirKind::Concat(subs) | HirKind::Alternation(subs) => {
            let mut changed = false;
            let relaxed: Vec<Hir> = subs
                .iter()
                .map(|sub| {
                    let (r, c) = relax(sub, aggressive);
                    changed |= c;
                    r
                })
                .collect();
            let hir = if matches!(hir.kind(), HirKind::Concat(_)) {
                Hir::concat(relaxed)
            } else {
                Hir::alternation(relaxed)
            };
            (hir, changed)
        }
        _ => (hir.clone(), false),
    }
}

/// Otzaria's `term_automaton::TermDfa`: one anchored dense DFA (a match is
/// decided on the end-of-input transition: whole-term).
#[derive(Debug)]
struct TermDfa {
    dfa: dense::DFA<Vec<u32>>,
    start: StateID,
}

/// Otzaria's `build_dfa` (+ its `check_supported`, which `relax` satisfies).
fn build_dfa(hir: &Hir) -> Result<TermDfa, String> {
    let nfa = thompson::Compiler::new()
        .configure(thompson::Config::new().nfa_size_limit(Some(NFA_SIZE_LIMIT)).which_captures(WhichCaptures::None))
        .build_from_hir(hir)
        .map_err(|e| format!("regex too large: {e}"))?;
    let dfa = dense::Builder::new()
        .configure(
            dense::Config::new()
                .match_kind(MatchKind::All)
                .start_kind(StartKind::Anchored)
                .minimize(false)
                .dfa_size_limit(Some(DFA_SIZE_LIMIT))
                .determinize_size_limit(Some(DETERMINIZE_SIZE_LIMIT)),
        )
        .build_from_nfa(&nfa)
        .map_err(|e| format!("regex DFA too large: {e}"))?;
    let start = dfa
        .start_state(&start::Config::new().anchored(Anchored::Yes))
        .map_err(|e| format!("DFA start state: {e}"))?;
    Ok(TermDfa { dfa, start })
}

/// One compiled regex word: the walk's DFA over the relaxed pattern, and the
/// verifier when relaxing let in terms the pattern rejects.
#[derive(Debug)]
struct TermPart {
    dfa: TermDfa,
    verify: Option<Regex>,
}

/// Otzaria's term-automaton cache bounds: entries and summed DFA memory.
const CACHE_ENTRIES: usize = 256;
const CACHE_BYTES: usize = 96 << 20;

type Compiled = Result<Arc<TermPart>, UserRegexError>;

#[derive(Default)]
struct PartCache {
    map: HashMap<String, (Compiled, usize)>,
    bytes: usize,
}

/// Otzaria's `prepare_term_part`, cached by pattern, failures included
/// (Otzaria caches its DFAs process-wide, bounded by entries and bytes;
/// here the whole cache is dropped when a bound is passed).
fn term_part(pattern: &str) -> Compiled {
    static CACHE: OnceLock<Mutex<PartCache>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some((c, _)) = cache.lock().unwrap_or_else(|e| e.into_inner()).map.get(pattern) {
        return c.clone();
    }
    let compiled = build_term_part(pattern);
    let size = match &compiled {
        Ok(p) => p.dfa.dfa.memory_usage() + pattern.len(),
        Err(_) => pattern.len(),
    };
    let mut c = cache.lock().unwrap_or_else(|e| e.into_inner());
    if c.map.len() >= CACHE_ENTRIES || c.bytes + size > CACHE_BYTES {
        c.map.clear();
        c.bytes = 0;
    }
    if size <= CACHE_BYTES {
        c.bytes += size;
        c.map.insert(pattern.to_string(), (compiled.clone(), size));
    }
    compiled
}

/// Otzaria's `term_automaton::check_supported`, after `relax`: a byte class
/// (`(?-u:...)`) is not a term regex (Otzaria: invalid, searched as text).
fn check_supported(hir: &Hir) -> Result<(), String> {
    match hir.kind() {
        HirKind::Look(_) => Err("empty-width assertions are not supported in term regexes".into()),
        HirKind::Class(Class::Bytes(_)) => Err("byte classes are not supported in term regexes".into()),
        HirKind::Repetition(rep) => check_supported(&rep.sub),
        HirKind::Capture(cap) => check_supported(&cap.sub),
        HirKind::Concat(subs) | HirKind::Alternation(subs) => subs.iter().try_for_each(check_supported),
        HirKind::Empty | HirKind::Literal(_) | HirKind::Class(_) => Ok(()),
    }
}

fn build_term_part(pattern: &str) -> Compiled {
    let exact = regex_syntax::Parser::new()
        .parse(pattern)
        .map_err(|e| UserRegexError::Invalid(format!("syntax: {e}")))?;
    let mut last_error = String::from("too large");
    for aggressive in [false, true] {
        let (relaxed, changed) = relax(&exact, aggressive);
        if cost(&relaxed) > TERM_COST_LIMIT {
            continue;
        }
        match check_supported(&relaxed).and_then(|()| build_dfa(&relaxed)) {
            Ok(dfa) => {
                let verify = if changed {
                    // Terms are made of the narrowed alphabet: the narrowed
                    // exact pattern decides them alike, and compiles smaller.
                    let narrowed = narrow(&exact);
                    if cost(&narrowed) > REGEX_COST_LIMIT {
                        return Err(UserRegexError::Invalid("too complex: bounded repetitions too large".into()));
                    }
                    Some(
                        RegexBuilder::new(&format!(r"\A(?:{narrowed})\z"))
                            .case_insensitive(true)
                            .size_limit(VERIFY_SIZE_LIMIT)
                            .dfa_size_limit(VERIFY_SIZE_LIMIT)
                            .build()
                            .map_err(|e| UserRegexError::Invalid(format!("too complex: {e}")))?,
                    )
                } else {
                    None
                };
                return Ok(Arc::new(TermPart { dfa, verify }));
            }
            Err(e) => last_error = e,
        }
    }
    Err(UserRegexError::Invalid(format!("too complex: {last_error}")))
}

/// The walk's automaton for one regex word. With a verifier the state also
/// carries the bytes read, and a match must pass it (Otzaria verifies each
/// collected term the same way; inside [`QuoteFree`] the quote-free run
/// reads the term without its quotes, as Otzaria's `verifies`).
#[derive(Debug, Clone)]
pub struct RegexTermAutomaton(Arc<TermPart>);

impl Automaton for RegexTermAutomaton {
    type State = (StateID, Option<Vec<u8>>);

    fn start(&self) -> Self::State {
        (self.0.dfa.start, self.0.verify.as_ref().map(|_| Vec::new()))
    }

    fn is_match(&self, state: &Self::State) -> bool {
        let dfa = &self.0.dfa.dfa;
        dfa.is_match_state(dfa.next_eoi_state(state.0))
            && match (&self.0.verify, &state.1) {
                (Some(v), Some(bytes)) => std::str::from_utf8(bytes).is_ok_and(|t| v.is_match(t)),
                _ => true,
            }
    }

    fn can_match(&self, state: &Self::State) -> bool {
        !self.0.dfa.dfa.is_dead_state(state.0)
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        let next = self.0.dfa.dfa.next_state(state.0, byte);
        let bytes = state.1.as_ref().map(|b| {
            let mut b = b.clone();
            b.push(byte);
            b
        });
        (next, bytes)
    }
}

/// One regex word as a walk over the term dictionary: whole-term, quote
/// transparent, capped at [`DEFAULT_MAX_EXPANDED_TERMS`] per segment,
/// shortest first.
#[derive(Debug, Clone)]
pub struct HebrewRegexQuery {
    field: Field,
    automaton: RegexTermAutomaton,
    matched_terms_sink: Option<MatchedTermsSink>,
    max_expansions: usize,
    /// The index keeps marks in its terms: they are read as zero width.
    marked_terms: bool,
}

impl HebrewRegexQuery {
    /// `pattern` is one word of [`UserRegex::words`].
    pub fn new(field: Field, pattern: &str) -> Result<HebrewRegexQuery, UserRegexError> {
        Ok(HebrewRegexQuery {
            field,
            automaton: RegexTermAutomaton(term_part(pattern)?),
            matched_terms_sink: None,
            max_expansions: DEFAULT_MAX_EXPANDED_TERMS,
            marked_terms: false,
        })
    }

    /// The index keeps marks in its terms (`keep_marks`): the folded pattern
    /// (marks dropped) then reads them as zero width
    /// ([`crate::marks::MarkFree`]), matching what it matches on the
    /// mark-stripped index.
    pub fn set_marked_terms(&mut self, marked_terms: bool) {
        self.marked_terms = marked_terms;
    }

    /// Collects the matched forms into `sink` during the search.
    pub fn set_matched_terms_sink(&mut self, sink: MatchedTermsSink) {
        self.matched_terms_sink = Some(sink);
    }

    /// Overrides the expansion cap.
    pub fn set_max_expansions(&mut self, cap: usize) {
        self.max_expansions = cap;
    }

    /// The automaton the walk runs.
    pub fn automaton(&self) -> QuoteFree<RegexTermAutomaton> {
        QuoteFree::new(self.automaton.clone(), true)
    }

    fn plan(&self) -> crate::marks::WeightPlan<'_> {
        crate::marks::WeightPlan {
            field: self.field,
            sink: self.matched_terms_sink.as_ref(),
            cap: Some(self.max_expansions),
            marked_terms: self.marked_terms,
            mark_free: self.marked_terms,
        }
    }

    /// This word as a span slot, capped per segment.
    pub fn span_slot(&self, _searcher: &tantivy::Searcher) -> tantivy::Result<Arc<dyn SpanTermSource>> {
        Ok(self.plan().span(self.automaton()))
    }
}

impl Query for HebrewRegexQuery {
    fn weight(&self, _enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(self.plan().boxed(self.automaton()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepts(pattern: &str, term: &str) -> bool {
        let a = QuoteFree::new(RegexTermAutomaton(term_part(pattern).unwrap()), true);
        let mut s = a.start();
        for &b in term.as_bytes() {
            s = a.accept(&s, b);
        }
        a.is_match(&s)
    }

    #[test]
    fn detection() {
        assert!(is_wrapped("/ab+/"));
        assert!(is_wrapped("  /a b/ "));
        // only slashes INSIDE the outer pair count (Otzaria): edge spaces are fine
        assert!(is_wrapped("/ ab/"));
        assert!(is_wrapped("/ab /"));
        assert!(!is_wrapped("/a/ /b/"));
        assert!(!is_wrapped("//"));
        assert!(!is_wrapped("ab"));
    }

    #[test]
    fn kinds() {
        assert_eq!(parse("/ab+c/").unwrap().words.len(), 1);
        assert_eq!(parse("/^ab c$/").unwrap().words.len(), 2);
        assert_eq!(parse("/ab  +c/").unwrap().words.len(), 2);
        assert_eq!(parse("/a\\sb/").unwrap().words.len(), 2); // \s separates words (Otzaria)
        assert_eq!(parse("/a[ ,]b/").unwrap_err(), UserRegexError::LineKind);
        // a merged Hebrew literal with a space is split into two words
        assert_eq!(parse("/\u{05D0}\u{05D1} \u{05D2}\u{05D3}/").unwrap().words.len(), 2);
        assert_eq!(parse("/a\\bb/").unwrap_err(), UserRegexError::LineKind);
        assert_eq!(parse("/ab .* cd/").unwrap_err(), UserRegexError::LineKind);
        assert!(matches!(parse("/a(b/"), Err(UserRegexError::Invalid(_))));
        // a byte class is not a term regex (Otzaria's check_supported)
        assert!(matches!(parse("/(?-u:\\w)+/"), Err(UserRegexError::Invalid(_))));
        assert!(matches!(parse("/^ $/"), Err(UserRegexError::Invalid(_))));
    }

    #[test]
    fn whole_term_case_folded_quote_transparent() {
        let w = &parse("/AB.D/").unwrap().words[0];
        assert!(accepts(w, "abxd"));
        assert!(!accepts(w, "abxdz"));
        // Hebrew: shin lamed vav? mem-sofit; marks in the pattern are dropped
        let w = &parse("/\u{05E9}\u{05B8}\u{05DC}\u{05D5}?\u{05DD}/").unwrap().words[0];
        assert!(accepts(w, "\u{05E9}\u{05DC}\u{05DD}"));
        assert!(accepts(w, "\u{05E9}\u{05DC}\u{05D5}\u{05DD}"));
        // a quote in the term is transparent
        let w = &parse("/\u{05E8}\u{05DE}\u{05D1}\u{05DD}/").unwrap().words[0];
        assert!(accepts(w, "\u{05E8}\u{05DE}\u{05D1}\"\u{05DD}"));
        // lazy quantifiers are walked greedy (Otzaria's relax)
        let w = &parse("/ab+?c/").unwrap().words[0];
        assert!(accepts(w, "abbbc"));
        // Unicode classes are narrowed to the term alphabet; bounded repeats
        // above 7 are unbounded for the walk and the verifier keeps the bound
        let w = &parse("/\\w{2,5}/").unwrap().words[0];
        assert!(accepts(w, "ab"));
        assert!(accepts(w, "\u{05D0}\u{05D1}\u{05D2}"));
        assert!(!accepts(w, "abcdef"));
        let w = &parse("/\\w{1,30}\u{05D5}\u{05EA}/").unwrap().words[0];
        assert!(accepts(w, "\u{05D0}\u{05D5}\u{05EA}"));
        let long: String = std::iter::repeat('\u{05D0}').take(31).chain("\u{05D5}\u{05EA}".chars()).collect();
        assert!(!accepts(w, &long));
        // a quote in the term is transparent under the verifier too
        assert!(accepts(w, "\u{05D0}\"\u{05D5}\u{05EA}"));
        // gershayim in the pattern folds to the index quote
        assert_eq!(fold_pattern("\u{05D0}\u{05F4}\u{05D1}"), "\u{05D0}\"\u{05D1}");
    }
}
