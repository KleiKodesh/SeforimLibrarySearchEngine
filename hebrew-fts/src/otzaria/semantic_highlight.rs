//! Semantic passage highlights (otzaria_search_engine 7.2.0 `semantic_highlight.rs` and
//! `SearchEngine::passage_highlight`), ported step for step with the model left out: the
//! engine cuts a line into clauses and picks the ones worth embedding ([`passage_plan`]),
//! the HOST embeds the query and those clauses with its model, and the engine marks the
//! clause nearest the query ([`passage_mark`]). Deterministic parts are byte-identical.

use std::collections::HashSet;
use std::ops::Range;

use super::smart_query;
use super::unicode_alnum::is_alphanumeric;

/// A line of this many words or fewer is shown whole already: nothing to point at.
pub const SHORT_LINE_WORDS: usize = 12;
/// A part shorter than this is merged with its neighbour.
const MIN_CLAUSE_WORDS: usize = 4;
/// A part longer than this is cut into overlapping windows.
const MAX_CLAUSE_WORDS: usize = 20;
const WINDOW_WORDS: usize = 14;
const WINDOW_STEP: usize = 10;
/// How far past a clause's edge a word of the query still joins its mark.
const EDGE_WORDS: usize = 3;
/// Clauses embedded per line, and per call: each is one inference.
pub const MAX_CLAUSES_PER_LINE: usize = 6;
pub const MAX_CLAUSES_PER_CALL: usize = 48;
/// `HighlightConfig::default().max_chars`: the snippet's byte budget.
pub const PASSAGE_SNIPPET_BYTES: usize = 800;

/// Characters a clause ends after.
const ENDS_AFTER: [char; 7] = ['.', ':', ';', '?', '!', '\u{05C3}', ','];

/// The byte ranges of `text`'s words within `range`: whitespace-separated runs holding a letter
/// or a digit, so that a lone punctuation mark is not a word.
fn words(text: &str, range: Range<usize>) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = None;
    let slice = &text[range.clone()];
    for (offset, c) in slice.char_indices() {
        let at = range.start + offset;
        match (c.is_whitespace(), start) {
            (true, Some(begin)) => {
                out.push(begin..at);
                start = None;
            }
            (false, None) => start = Some(at),
            _ => {}
        }
    }
    if let Some(begin) = start {
        out.push(begin..range.end);
    }
    out.retain(|word| text[word.clone()].chars().any(is_alphanumeric));
    out
}

/// `text` cut at its punctuation: after `. : ; ? ! sof-pasuq ,`, before `(`, after `)` and after a dash.
fn raw_parts(text: &str) -> Vec<Range<usize>> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut previous: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let next = chars.peek().map(|&(_, c)| c);
        let cut = match c {
            '(' => Some(at),
            ')' | '\u{2013}' | '\u{2014}' => Some(at + c.len_utf8()),
            // A hyphen inside a word joins it; only a free-standing one separates.
            '-' if previous.is_none_or(char::is_whitespace) || next.is_none_or(char::is_whitespace) => {
                Some(at + c.len_utf8())
            }
            c if ENDS_AFTER.contains(&c) => Some(at + c.len_utf8()),
            _ => None,
        };
        if let Some(cut) = cut {
            if cut > start {
                parts.push(start..cut);
            }
            start = cut;
        }
        previous = Some(c);
    }
    if start < text.len() {
        parts.push(start..text.len());
    }
    parts
}

/// Byte ranges (first word to last) of `text`'s clauses, each with the part it was cut from;
/// empty for a line of at most twelve words or of a single clause.
pub fn clauses_in_parts(text: &str) -> Vec<(Range<usize>, Range<usize>)> {
    if words(text, 0..text.len()).len() <= SHORT_LINE_WORDS {
        return Vec::new();
    }
    let mut merged: Vec<(Range<usize>, usize)> = Vec::new();
    for part in raw_parts(text) {
        let count = words(text, part.clone()).len();
        match merged.last_mut() {
            Some((last, last_count)) if *last_count < MIN_CLAUSE_WORDS => {
                last.end = part.end;
                *last_count += count;
            }
            _ => merged.push((part, count)),
        }
    }
    if merged.len() > 1 && merged.last().is_some_and(|(_, n)| *n < MIN_CLAUSE_WORDS) {
        let (last, _) = merged.pop().expect("checked above");
        merged.last_mut().expect("checked above").0.end = last.end;
    }

    let mut out = Vec::new();
    for (part, _) in merged {
        let part_words = words(text, part);
        if part_words.is_empty() {
            continue;
        }
        let whole = part_words[0].start..part_words[part_words.len() - 1].end;
        if part_words.len() <= MAX_CLAUSE_WORDS {
            out.push((whole.clone(), whole));
            continue;
        }
        let mut first = 0;
        loop {
            let last = (first + WINDOW_WORDS).min(part_words.len()) - 1;
            out.push((part_words[first].start..part_words[last].end, whole.clone()));
            if last + 1 >= part_words.len() {
                break;
            }
            first += WINDOW_STEP;
        }
    }
    if out.len() < 2 {
        return Vec::new();
    }
    out
}

/// `span` widened, inside `part`, through the nearest [`EDGE_WORDS`] words on each side up to
/// the farthest that `is_query_word` accepts.
fn widen_to_query_words(text: &str, span: Range<usize>, part: Range<usize>, is_query_word: impl Fn(&str) -> bool) -> Range<usize> {
    let after = words(text, span.end..part.end.max(span.end));
    let end = after
        .iter()
        .take(EDGE_WORDS)
        .rposition(|word| is_query_word(&text[word.clone()]))
        .map_or(span.end, |index| after[index].end);
    let before = words(text, part.start.min(span.start)..span.start);
    let start = before
        .iter()
        .rev()
        .take(EDGE_WORDS)
        .rposition(|word| is_query_word(&text[word.clone()]))
        .map_or(span.start, |index| before[before.len() - 1 - index].start);
    start..end
}

/// The indices of at most `cap` clauses worth embedding, in order: the first clause always, and
/// those that share the most of `wanted` with the query, the earlier on a tie.
fn preselect(clause_tokens: &[Vec<String>], wanted: &HashSet<String>, cap: usize) -> Vec<usize> {
    if cap == 0 || clause_tokens.is_empty() {
        return Vec::new();
    }
    let overlap = |tokens: &Vec<String>| tokens.iter().filter(|t| wanted.contains(*t)).count();
    let mut rest: Vec<(usize, usize)> =
        clause_tokens.iter().enumerate().skip(1).map(|(index, tokens)| (index, overlap(tokens))).collect();
    rest.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut chosen: Vec<usize> = std::iter::once(0).chain(rest.into_iter().map(|(index, _)| index)).take(cap).collect();
    chosen.sort_unstable();
    chosen
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut aa, mut bb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        dot += f64::from(*x) * f64::from(*y);
        aa += f64::from(*x) * f64::from(*x);
        bb += f64::from(*y) * f64::from(*y);
    }
    let norm = (aa * bb).sqrt();
    if norm > 0.0 {
        (dot / norm) as f32
    } else {
        0.0
    }
}

/// The position in `vectors` nearest `query` by cosine, and that cosine; the first on a tie.
fn nearest(query: &[f32], vectors: &[Vec<f32>]) -> Option<(usize, f32)> {
    let mut best: Option<(usize, f32)> = None;
    for (index, vector) in vectors.iter().enumerate() {
        let score = cosine(query, vector);
        if best.is_none_or(|(_, top)| score > top) {
            best = Some((index, score));
        }
    }
    best
}

/// search_engine.rs `crop_around_first_occurrence`: `text` cut to at most `budget` bytes
/// around its first occurrence, keeping whole context words.
fn crop_around_first_occurrence<'a>(
    text: &'a str,
    ranges: &[(usize, usize)],
    word_count: usize,
    budget: usize,
) -> Option<(&'a str, Vec<(usize, usize)>)> {
    let occ_start = ranges.first()?.0;
    let occ_end = ranges.get(word_count.checked_sub(1)?)?.1;
    let extra = budget.checked_sub(occ_end - occ_start)?;
    let mut start = occ_start.saturating_sub(extra / 2);
    let mut end = (occ_end + extra - (occ_start - start)).min(text.len());
    start = start.saturating_sub(budget.saturating_sub(end - start));
    while start > 0 && !text.is_char_boundary(start) {
        start += 1;
    }
    while end < text.len() && !text.is_char_boundary(end) {
        end -= 1;
    }
    if start > 0 && !text[..start].ends_with(char::is_whitespace) {
        start = text[start..occ_start]
            .char_indices()
            .find(|&(_, c)| c.is_whitespace())
            .map_or(occ_start, |(offset, c)| start + offset + c.len_utf8());
    }
    if end < text.len() && !text[end..].starts_with(char::is_whitespace) {
        end = text[occ_end..end].rfind(char::is_whitespace).map_or(occ_end, |offset| occ_end + offset);
    }
    let cropped = ranges
        .chunks_exact(word_count)
        .filter(|occurrence| occurrence.iter().all(|&(s, e)| s >= start && e <= end))
        .flatten()
        .map(|&(s, e)| (s - start, e - start))
        .collect();
    Some((&text[start..end], cropped))
}

/// htmlescape 0.3.1 `encode_minimal`.
fn encode_minimal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("&quot;"),
            '&' => out.push_str("&amp;"),
            '\'' => out.push_str("&#x27;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// `marked_snippet_at`: the source window and its html, the span in `<mark>`.
fn marked_snippet_at(text: &str, span: Range<usize>, budget: usize) -> Option<(Range<usize>, String)> {
    let (window, ranges) = crop_around_first_occurrence(text, &[(span.start, span.end)], 1, budget)?;
    let &(start, end) = ranges.first()?;
    let offset = window.as_ptr() as usize - text.as_ptr() as usize;
    let mut html = String::with_capacity(window.len() + 32);
    html.push_str(&encode_minimal(&window[..start]));
    html.push_str("<mark>");
    html.push_str(&encode_minimal(&window[start..end]));
    html.push_str("</mark>");
    html.push_str(&encode_minimal(&window[end..]));
    Some((offset..offset + window.len(), html))
}

fn with_ellipsis(mut html: String, leading: bool, trailing: bool) -> String {
    if leading {
        html.insert(0, '\u{2026}');
    }
    if trailing {
        html.push('\u{2026}');
    }
    html
}

/// `quoteless_variant`: a token's spelling without `"` / `'`, when it has any.
fn quoteless_variant(token: &str) -> Option<String> {
    if !token.contains(['"', '\'']) {
        return None;
    }
    let clean: String = token.chars().filter(|c| !matches!(c, '"' | '\'')).collect();
    (!clean.is_empty()).then_some(clean)
}

/// The query's index tokens and their smart-query alternatives (`PassageQuery::tokens`).
fn query_tokens(query: &str) -> HashSet<String> {
    smart_query::display_tokens(query).iter().flat_map(|token| smart_query::alternatives(token)).collect()
}

/// One line's plan: which clauses the host embeds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PassageLinePlan {
    /// Indices into [`clauses_in_parts`] of the clauses to embed, in order; empty when the
    /// line has nothing to mark or the call's clauses ran out before it.
    pub chosen: Vec<usize>,
    /// The texts of those clauses (what the host embeds as passages).
    pub clauses: Vec<String>,
    /// True when the line had clauses but the call's budget ran out before it (Otzaria
    /// returns no highlight then and does not remember it).
    pub out_of_budget: bool,
}

/// The plan of a highlight call.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PassagePlan {
    /// The query as the host embeds it (`smart_query::canonical`).
    pub query_text: String,
    /// True when some line has clauses to embed, so the query vector is needed.
    pub needs_query_vector: bool,
    pub lines: Vec<PassageLinePlan>,
}

/// `passage_highlights_with` + `passage_highlight` up to the embedding: each text's clauses,
/// the call budget ([`MAX_CLAUSES_PER_CALL`]) spent in order, and the clauses chosen by the
/// words they share with the query when a line has more than it embeds. `forms`: the host
/// dictionary's highlight forms of the query words (Otzaria's `magic_dict.highlight_forms`),
/// added to the words a clause is chosen by.
pub fn passage_plan(query: &str, texts: &[&str], forms: &[String]) -> PassagePlan {
    let mut words: Option<HashSet<String>> = None;
    let mut left = MAX_CLAUSES_PER_CALL;
    let mut plan = PassagePlan { query_text: smart_query::canonical(query).into_owned(), ..Default::default() };
    for text in texts {
        let clauses: Vec<Range<usize>> = clauses_in_parts(text).into_iter().map(|(clause, _)| clause).collect();
        if clauses.is_empty() {
            plan.lines.push(PassageLinePlan::default());
            continue;
        }
        let cap = MAX_CLAUSES_PER_LINE.min(left);
        if cap < 2 {
            plan.lines.push(PassageLinePlan { out_of_budget: true, ..Default::default() });
            continue;
        }
        let chosen: Vec<usize> = if clauses.len() <= cap {
            (0..clauses.len()).collect()
        } else {
            let wanted = words.get_or_insert_with(|| {
                let mut set = HashSet::new();
                for token in smart_query::display_tokens(query).iter().flat_map(|token| smart_query::alternatives(token)) {
                    if let Some(clean) = quoteless_variant(&token) {
                        set.insert(clean);
                    }
                    set.insert(token);
                }
                set.extend(forms.iter().cloned());
                set
            });
            let tokens: Vec<Vec<String>> = clauses.iter().map(|clause| smart_query::display_tokens(&text[clause.clone()])).collect();
            preselect(&tokens, wanted, cap)
        };
        left -= chosen.len();
        plan.needs_query_vector = true;
        plan.lines.push(PassageLinePlan {
            clauses: chosen.iter().map(|&i| text[clauses[i].clone()].to_string()).collect(),
            chosen,
            out_of_budget: false,
        });
    }
    plan
}

/// A marked line: the snippet html and the cosine of the marked clause.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PassageHighlight {
    /// Escaped html, the clause in `<mark>`, `…` where cut; empty when nothing is marked.
    pub snippet_html: String,
    pub span_score: Option<f32>,
}

/// `passage_highlight` after the embedding: the chosen clause nearest `query_vector`,
/// widened to query words within three words of its edge, cut to `budget` bytes around it.
/// `chosen` and `vectors` (one per chosen clause, in order) come from [`passage_plan`] and the
/// host's model. An empty result for a line with nothing to mark.
pub fn passage_mark(query: &str, text: &str, chosen: &[usize], vectors: &[Vec<f32>], query_vector: &[f32], budget: usize) -> PassageHighlight {
    let (clauses, parts): (Vec<_>, Vec<_>) = clauses_in_parts(text).into_iter().unzip();
    if clauses.is_empty() || chosen.iter().any(|&i| i >= clauses.len()) {
        return PassageHighlight::default();
    }
    let Some((best, score)) = nearest(query_vector, vectors) else {
        return PassageHighlight::default();
    };
    let Some(&index) = chosen.get(best) else {
        return PassageHighlight::default();
    };
    let tokens = query_tokens(query);
    let span = widen_to_query_words(text, clauses[index].clone(), parts[index].clone(), |word| {
        smart_query::display_tokens(word).iter().any(|token| tokens.contains(token))
    });
    match marked_snippet_at(text, span, budget) {
        Some((window, html)) => PassageHighlight {
            snippet_html: with_ellipsis(html, window.start > 0, window.end < text.len()),
            span_score: Some(score),
        },
        None => PassageHighlight::default(),
    }
}

#[cfg(test)]
mod tests {
    // The old engine's own tests (otzaria_search_engine 7.2.0 semantic_highlight.rs), verbatim
    // but the cache test (the cache stays with the host).
    use super::*;

    fn texts<'a>(text: &'a str, ranges: &[Range<usize>]) -> Vec<&'a str> {
        ranges.iter().map(|range| &text[range.clone()]).collect()
    }

    #[test]
    fn a_short_line_has_no_clauses() {
        assert!(clauses("אחת שתים שלש, ארבע חמש שש שבע. שמונה תשע עשר אחת עשרה").is_empty());
        assert!(clauses("").is_empty());
    }

    #[test]
    fn a_line_is_cut_after_hebrew_punctuation_and_at_parentheses_and_dashes() {
        let text =
            "אמר רבי יוחנן הלכה כרבי מאיר׃ ואמר רבי אלעזר בן עזריה בשמו (כדאיתא בברכות דף ב) \
                    ועוד אמרו חכמים זכרונם לברכה – שכל המקיים מצוה זו כראוי; ומה טעם בדבר?";
        assert_eq!(
            texts(text, &clauses(text)),
            [
                "אמר רבי יוחנן הלכה כרבי מאיר׃",
                "ואמר רבי אלעזר בן עזריה בשמו",
                "(כדאיתא בברכות דף ב)",
                "ועוד אמרו חכמים זכרונם לברכה",
                "שכל המקיים מצוה זו כראוי; ומה טעם בדבר?",
            ]
        );
    }

    #[test]
    fn short_parts_merge_forward_and_the_last_backward() {
        let text = "אמר, רבי יוחנן הלכה כרבי מאיר בכל מקום, ואמר רבי אלעזר כן הוא. סוף דבר.";
        assert_eq!(
            texts(text, &clauses(text)),
            [
                "אמר, רבי יוחנן הלכה כרבי מאיר בכל מקום,",
                "ואמר רבי אלעזר כן הוא. סוף דבר.",
            ]
        );
    }

    #[test]
    fn a_hyphen_inside_a_word_does_not_cut() {
        let text = "בן-אדם הולך בדרך הישר והטוב, ושומר את כל המצוות - ואין עוד מלבדו כלל ועיקר";
        assert_eq!(
            texts(text, &clauses(text)),
            [
                "בן-אדם הולך בדרך הישר והטוב,",
                "ושומר את כל המצוות",
                "ואין עוד מלבדו כלל ועיקר",
            ]
        );
    }

    #[test]
    fn a_long_part_becomes_overlapping_windows() {
        let words: Vec<String> = (1..=27).map(|n| format!("מילה{n}")).collect();
        let text = words.join(" ");
        let got = texts(&text, &clauses(&text));
        assert_eq!(
            got,
            [
                words[0..14].join(" "),
                words[10..24].join(" "),
                words[20..27].join(" "),
            ]
        );
    }

    #[test]
    fn a_window_takes_in_a_word_of_the_query_just_past_its_edge() {
        let words: Vec<String> = (1..=27).map(|n| format!("מילה{n}")).collect();
        let text = words.join(" ");
        let found = clauses_in_parts(&text);
        let (first, part) = found[0].clone();
        assert_eq!(part, 0..text.len());
        let query = |wanted: &'static [&'static str]| move |word: &str| wanted.contains(&word);
        let widened = |wanted| {
            texts(
                &text,
                &[widen_to_query_words(
                    &text,
                    first.clone(),
                    part.clone(),
                    query(wanted),
                )],
            )[0]
            .to_string()
        };
        assert_eq!(widened(&["מילה16"]), words[0..16].join(" "));
        assert_eq!(widened(&["מילה15", "מילה17"]), words[0..17].join(" "));
        assert_eq!(widened(&["מילה18"]), words[0..14].join(" "), "too far");
        let (second, _) = found[1].clone();
        let back = widen_to_query_words(&text, second, part, query(&["מילה9"]));
        assert_eq!(&text[back], words[8..24].join(" "));

        let line = "אמר רבי יוחנן הלכה כרבי מאיר בכל מקום, ואמר רבי אלעזר כן הוא ומה טעם בדבר זה.";
        let (clause, part) = clauses_in_parts(line)[0].clone();
        assert_eq!(
            widen_to_query_words(line, clause.clone(), part, query(&["ואמר"])),
            clause,
            "punctuation ends a clause"
        );
    }

    #[test]
    fn a_line_of_one_clause_has_nothing_to_choose() {
        let words: Vec<String> = (1..=16).map(|n| format!("מילה{n}")).collect();
        assert!(clauses(&words.join(" ")).is_empty());
    }

    #[test]
    fn preselection_keeps_the_first_clause_and_the_best_overlaps_in_order() {
        let tokens: Vec<Vec<String>> = [
            &["א", "ב"][..],
            &["שבת", "קודש"],
            &["ג"],
            &["שבת"],
            &["ד"],
            &["קודש", "שבת", "שבת"],
            &["ה"],
            &["ו"],
        ]
        .iter()
        .map(|clause| clause.iter().map(|t| t.to_string()).collect())
        .collect();
        let wanted: HashSet<String> = ["שבת", "קודש"].map(String::from).into();
        assert_eq!(preselect(&tokens, &wanted, 3), [0, 1, 5]);
        assert_eq!(preselect(&tokens, &wanted, 6), [0, 1, 2, 3, 4, 5]);
        assert_eq!(preselect(&tokens, &wanted, 0), [] as [usize; 0]);
    }

    #[test]
    fn the_nearest_vector_wins_and_a_tie_goes_to_the_first() {
        let query = [1.0, 0.0];
        let vectors = vec![
            vec![0.0, 1.0],
            vec![1.0, 1.0],
            vec![2.0, 2.0],
            vec![-1.0, 0.0],
        ];
        let (index, score) = nearest(&query, &vectors).unwrap();
        assert_eq!(index, 1);
        assert!((score - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        assert_eq!(nearest(&query, &[]), None);
    }

    #[test]
    fn the_snippet_is_centred_escaped_and_cut_on_words() {
        let before = "אלף בית גימל דלת הא וו זין חית טית יוד ".repeat(4);
        let after = " כף למד מם נון סמך עין פא צדי קוף ריש".repeat(4);
        let text = format!("{before}<מצוה> & גדולה{after}");
        let start = before.len();
        let end = start + "<מצוה> & גדולה".len();
        let html = marked_snippet(&text, start..end, 120).unwrap();
        assert!(html.starts_with('…') && html.ends_with('…'), "{html}");
        assert!(
            html.contains("<mark>&lt;מצוה&gt; &amp; גדולה</mark>"),
            "{html}"
        );
        // Whole words at both cuts.
        let inner = html.trim_matches('…');
        let shown = inner.replace("<mark>", "").replace("</mark>", "");
        let shown = shown
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&");
        assert!(text.contains(&shown), "{shown}");
        let first = shown.split(' ').next().unwrap();
        let last = shown.split(' ').next_back().unwrap();
        assert!(text.split(' ').any(|w| w == first), "{first}");
        assert!(text.split(' ').any(|w| w == last), "{last}");
        // Centred: the two sides differ by about a word at most.
        let (left, right) = inner.split_once("<mark>").unwrap();
        let right = right.split_once("</mark>").unwrap().1;
        assert!(left.len().abs_diff(right.len()) <= 20, "{left} | {right}");

        assert_eq!(
            marked_snippet("קצר מאוד", 0.."קצר".len(), 800).unwrap(),
            "<mark>קצר</mark> מאוד"
        );
        assert_eq!(marked_snippet(&text, start..end, 8), None);
    }

    /// The old engine's test helper `marked_snippet`.
    fn marked_snippet(text: &str, span: Range<usize>, budget: usize) -> Option<String> {
        marked_snippet_at(text, span, budget).map(|(window, html)| with_ellipsis(html, window.start > 0, window.end < text.len()))
    }



    fn clauses(text: &str) -> Vec<Range<usize>> {
        clauses_in_parts(text).into_iter().map(|(c, _)| c).collect()
    }

    #[test]
    fn short_lines_and_single_clauses_have_no_clauses() {
        assert!(clauses("one two three four five six seven eight nine ten eleven twelve").is_empty());
        assert!(clauses("one two three four five six seven eight nine ten eleven twelve thirteen").is_empty());
    }

    #[test]
    fn lines_cut_at_punctuation_and_short_parts_merge() {
        let text = "alpha beta gamma delta epsilon, zeta eta theta iota kappa; lambda mu. nu xi omicron pi rho";
        assert_eq!(
            texts(text, &clauses(text)),
            ["alpha beta gamma delta epsilon,", "zeta eta theta iota kappa;", "lambda mu. nu xi omicron pi rho"]
        );
    }

    #[test]
    fn long_parts_become_overlapping_windows() {
        let words: Vec<String> = (0..30).map(|i| format!("w{i}")).collect();
        let text = words.join(" ");
        let got = texts(&text, &clauses(&text));
        assert_eq!(got.len(), 3);
        assert!(got[0].starts_with("w0 ") && got[0].ends_with(" w13"));
        assert!(got[1].starts_with("w10 ") && got[1].ends_with(" w23"));
        assert!(got[2].starts_with("w20 ") && got[2].ends_with(" w29"));
    }

    #[test]
    fn preselect_keeps_the_first_and_the_best_overlaps() {
        let tokens: Vec<Vec<String>> =
            vec![vec!["a".into()], vec!["x".into()], vec!["q".into(), "r".into()], vec!["q".into()]];
        let wanted: HashSet<String> = ["q".to_string(), "r".to_string()].into();
        assert_eq!(preselect(&tokens, &wanted, 3), [0, 2, 3]);
        assert_eq!(preselect(&tokens, &wanted, 0), Vec::<usize>::new());
    }

    #[test]
    fn plan_spends_the_call_budget_in_order() {
        let words: Vec<String> = (0..120).map(|i| format!("w{i},")).collect();
        let line = words.join(" ");
        let texts: Vec<&str> = vec![line.as_str(); 10];
        let plan = passage_plan("w1", &texts, &[]);
        let spent: Vec<usize> = plan.lines.iter().map(|l| l.chosen.len()).collect();
        assert_eq!(spent, [6, 6, 6, 6, 6, 6, 6, 6, 0, 0]);
        assert!(plan.lines[8].out_of_budget);
        assert!(plan.needs_query_vector);
        let short = passage_plan("w1", &["a b c"], &[]);
        assert!(!short.needs_query_vector && !short.lines[0].out_of_budget);
    }

    #[test]
    fn mark_wraps_the_nearest_clause_and_escapes() {
        let text = "alpha beta gamma delta epsilon, zeta eta theta iota <kappa>; lambda mu. nu xi omicron pi rho";
        let plan = passage_plan("theta", &[text], &[]);
        let line = &plan.lines[0];
        assert_eq!(line.chosen, [0, 1, 2]);
        let vectors = vec![vec![1.0, 0.0], vec![0.0, 1.0], vec![0.6, 0.8]];
        let got = passage_mark("theta", text, &line.chosen, &vectors, &[0.0, 1.0], PASSAGE_SNIPPET_BYTES);
        assert_eq!(
            got.snippet_html,
            "alpha beta gamma delta epsilon, <mark>zeta eta theta iota &lt;kappa&gt;;</mark> lambda mu. nu xi omicron pi rho"
        );
        assert_eq!(got.span_score, Some(1.0));
        let cut = passage_mark("theta", text, &line.chosen, &vectors, &[0.0, 1.0], 50);
        assert!(cut.snippet_html.starts_with('\u{2026}') || cut.snippet_html.ends_with('\u{2026}'));
    }
}
