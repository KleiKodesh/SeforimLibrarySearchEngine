//! Otzaria's own search test cases (otzaria_search_engine v5.0.0,
//! `rust/src/api/search_engine.rs` and `hebrew_query.rs`), run against our
//! engine. Each case indexes Otzaria's inline lines, runs the search the way
//! an Otzaria-mode host would, and checks which lines match. A line matches
//! when the engine hits it AND its snippet locates (the host-side filters:
//! word distance, order, exclusion on text).
//!
//! Every case runs on an index WITH positions and WITHOUT. A failing case is
//! not assumed to be our bug or theirs: it is analysed first.
//!
//! Words are Otzaria's (FtsLib's word model is Otzaria's): one-letter words
//! and digit runs are words, as in Otzaria's index.
//!
//! Word distance goes through the engine's own Otzaria rule,
//! `SearchRequest::proximity` (`ProximitySpec`): ordered with per-pair gaps,
//! or any order inside one window. With positions it runs as a span query;
//! without, the snippet locator validates it on the text.

use seforim_engine::{
    ExcludeOptions, ExcludeSpec, GroupSpec, IndexOptions, LineDoc, LineIndex, PhraseOptions, ProximitySpec,
    SearchOptions, SearchRequest, SearchResult, TypoModeSpec, WalkOptionsSpec, WordOptions,
};

fn w(word: &str) -> GroupSpec {
    GroupSpec::Word { word: word.into(), options: WalkOptionsSpec::default() }
}

fn wo(word: &str, f: impl FnOnce(&mut WalkOptionsSpec)) -> GroupSpec {
    let mut o = WalkOptionsSpec::default();
    f(&mut o);
    GroupSpec::Word { word: word.into(), options: o }
}

/// Matching line numbers (1-based, in the case's line order) for `req`.
fn matches(lines: &[&str], positions: bool, req: &SearchRequest) -> Vec<usize> {
    located(lines, positions, req).into_iter().map(|(line, _)| line).collect()
}

/// Matching lines with their located window `(start, end)`.
fn located(lines: &[&str], positions: bool, req: &SearchRequest) -> Vec<(usize, (usize, usize))> {
    located_with(lines, positions, |index| index.search(req).unwrap())
}

/// Matching lines for the public [`SearchOptions`] API.
fn matches_options(lines: &[&str], positions: bool, options: &SearchOptions) -> Vec<usize> {
    located_with(lines, positions, |index| index.search_options(options).unwrap())
        .into_iter()
        .map(|(line, _)| line)
        .collect()
}

fn located_with(
    lines: &[&str],
    positions: bool,
    search: impl Fn(&LineIndex) -> SearchResult,
) -> Vec<(usize, (usize, usize))> {
    let dir = tempfile::tempdir().unwrap();
    let index = LineIndex::create(dir.path(), IndexOptions { positions, keep_marks: false }).unwrap();
    let mut wr = index.writer(30_000_000, 1).unwrap();
    for (i, t) in lines.iter().enumerate() {
        wr.add(&LineDoc { line_id: i as u64 + 1, position: i as u64, book_id: 1, text: (*t).into(), section_id: None, no_cross: false })
            .unwrap();
    }
    wr.commit(None).unwrap();
    let mut r = search(&index);
    let mut out = Vec::new();
    for h in r.hits.clone() {
        let text = lines[h.line_id as usize - 1];
        if let Some(s) = r.session.locate(text) {
            out.push((h.line_id as usize, (s.start, s.end)));
        }
    }
    out.sort();
    out
}

/// Otzaria "ordered, distance d": every pair at most d words apart.
fn ordered(groups: Vec<GroupSpec>, d: u32) -> SearchRequest {
    let gaps = vec![d; groups.len().saturating_sub(1)];
    spacing(groups, gaps)
}

/// Otzaria's customSpacing: one limit per consecutive pair, in order.
fn spacing(groups: Vec<GroupSpec>, gaps: Vec<u32>) -> SearchRequest {
    SearchRequest { groups, proximity: Some(ProximitySpec::Ordered { gaps }), ..Default::default() }
}

/// Otzaria "any order, distance d".
fn any_order(groups: Vec<GroupSpec>, d: u32) -> SearchRequest {
    SearchRequest { groups, proximity: Some(ProximitySpec::AnyOrder { distance: d }), ..Default::default() }
}

enum Expect {
    /// exactly these lines
    Exactly(&'static [usize]),
    /// at least these lines, and none of the second list
    Contains(&'static [usize], &'static [usize]),
}

struct Case {
    name: &'static str,
    lines: &'static [&'static str],
    req: fn() -> SearchRequest,
    expect: Expect,
    /// needs an index with positions (none today: kept for future cases)
    positions_only: bool,
}

const ARAMAIC: &[&str] = &[
    "מלכא קדישא",
    "אמרו דמלכא הוא",
    "כדמלכה בשעתו",
    "מדמלכא נפק",
    "אדמלכה קאי",
    "חכמין אמרין",
    "ספרא אחרינא",
];
const RAMBAM: &[&str] = &["כתב רמב\"ם על כך", "דברי רמבם בהלכות"];
const RAMBAM2: &[&str] = &["דברי רמבם בהלכות", "כתב הרמב\"ם על כך"];
const ANY_ORDER: &[&str] = &[
    "חייב להחזיר",
    "להחזיר חייב",
    "להחזיר את החפץ חייב",
    "להחזיר את החפץ הזה חייב",
    "חייב לשלם ולא להחזיר",
    "אמר רבי יוחנן",
    "אמר רבי אמר",
];
const SCOPE: &[&str] = &[
    "דין אינו מתכוין בשבת",
    "ודין מתעסק בחלבים ועריות",
    "כאן נדון רק במלאכת שבת",
    "מתעסק וגם אינו מתכוין באותה שורה",
];

// Otzaria 7.0.1 typo cases (issue #36).
const PALM: &[&str] = &[
    "\u{05E4}\u{05D0}\u{05DC}\u{05DE}\u{05D9}\u{05D8}\u{05D5}: \u{05DC}\u{05D1}\u{05D1}\u{05D5}\u{05EA} \u{05D3}\u{05E7}\u{05DC} (hearts of palm)",
    "\u{05DC}\u{05D1}\u{05D5}\u{05EA} \u{05D3}\u{05E7}\u{05DC}",
];
const PALMITO: &str = "\u{05E4}\u{05D0}\u{05DC}\u{05DE}\u{05D9}\u{05D8}\u{05D5}";
const LLBVT: &str = "\u{05DC}\u{05DC}\u{05D1}\u{05D5}\u{05EA}";
const DQL: &str = "\u{05D3}\u{05E7}\u{05DC}";
const SHALOM: &str = "\u{05E9}\u{05DC}\u{05D5}\u{05DD}";

fn typo(o: &mut WalkOptionsSpec) {
    o.spelling_errors = true;
}

fn typo_gram(o: &mut WalkOptionsSpec) {
    o.spelling_errors = true;
    o.grammar_prefixes = true;
}

fn cases() -> Vec<Case> {
    use Expect::*;
    vec![
        Case { name: "grammatical prefix ספר", lines: &["ספר", "הספר", "מטבע"],
            req: || SearchRequest { groups: vec![wo("ספר", |o| o.grammar_prefixes = true)], ..Default::default() },
            expect: Exactly(&[1, 2]), positions_only: false },
        Case { name: "no options ספר", lines: &["ספר", "הספר", "מטבע"],
            req: || SearchRequest { groups: vec![w("ספר")], ..Default::default() },
            expect: Exactly(&[1]), positions_only: false },
        Case { name: "alternative words שר|מלך", lines: &["מלך", "שר", "עיר"],
            req: || SearchRequest { groups: vec![GroupSpec::Any { alternatives: vec![w("שר"), w("מלך")] }], ..Default::default() },
            expect: Exactly(&[1, 2]), positions_only: false },
        Case { name: "aramaic prefixes+suffixes מלכה", lines: ARAMAIC,
            req: || SearchRequest { groups: vec![wo("מלכה", |o| { o.aramaic_prefixes = true; o.aramaic_suffixes = true; })], ..Default::default() },
            expect: Contains(&[1, 2, 3, 4, 5], &[7]), positions_only: false },
        Case { name: "aramaic suffixes only מלכה", lines: ARAMAIC,
            req: || SearchRequest { groups: vec![wo("מלכה", |o| o.aramaic_suffixes = true)], ..Default::default() },
            expect: Contains(&[1], &[2, 3, 4, 5]), positions_only: false },
        Case { name: "aramaic suffixes חכמים", lines: ARAMAIC,
            req: || SearchRequest { groups: vec![wo("חכמים", |o| o.aramaic_suffixes = true)], ..Default::default() },
            expect: Contains(&[6], &[]), positions_only: false },
        Case { name: "רמבם plain (Otzaria: quote-free twin token)", lines: RAMBAM,
            req: || SearchRequest { groups: vec![w("רמבם")], ..Default::default() },
            expect: Exactly(&[1, 2]), positions_only: false },
        Case { name: "רמב\"ם plain", lines: RAMBAM,
            req: || SearchRequest { groups: vec![w("רמב\"ם")], ..Default::default() },
            expect: Exactly(&[1]), positions_only: false },
        Case { name: "רמב\"ם ignore gershayim", lines: RAMBAM,
            req: || SearchRequest { groups: vec![wo("רמב\"ם", |o| o.ignore_quotes = true)], ..Default::default() },
            expect: Exactly(&[1, 2]), positions_only: false },
        Case { name: "רמב\"ם typos", lines: RAMBAM2,
            req: || SearchRequest { groups: vec![wo("רמב\"ם", |o| o.spelling_errors = true)], ..Default::default() },
            expect: Contains(&[1], &[]), positions_only: false },
        Case { name: "רמב\"ם grammatical prefixes", lines: RAMBAM2,
            req: || SearchRequest { groups: vec![wo("רמב\"ם", |o| o.grammar_prefixes = true)], ..Default::default() },
            expect: Contains(&[2], &[]), positions_only: false },
        // Otzaria advanced_typo_only_single_word_covers_full_edit_distance.
        Case { name: "typo ברכה: full edit distance 1", lines: &["ברכה בראשית", "ברכגה אחר", "תורה צוה"],
            req: || SearchRequest { groups: vec![wo("ברכה", |o| o.spelling_errors = true)], ..Default::default() },
            expect: Exactly(&[1, 2]), positions_only: false },
        // TypoMode::Otzaria (engine 7.0.1): with another option, the sampled
        // list plus every single-letter edit of the typed form (the ג
        // insertion); FullEdit1: the full edit distance 1.
        Case { name: "typo + grammatical prefixes ברכה (Otzaria 7.0.1)", lines: &["ברכה", "ברכגה", "והברקה", "תורה"],
            req: || SearchRequest { groups: vec![wo("ברכה", |o| { o.spelling_errors = true; o.grammar_prefixes = true; })], ..Default::default() },
            expect: Exactly(&[1, 2, 3]), positions_only: false },
        Case { name: "typo + grammatical prefixes ברכה (FullEdit1)", lines: &["ברכה", "ברכגה", "והברקה", "תורה"],
            req: || SearchRequest { groups: vec![wo("ברכה", |o| { o.spelling_errors = true; o.grammar_prefixes = true; o.typo_mode = TypoModeSpec::FullEdit1; })], ..Default::default() },
            expect: Exactly(&[1, 2, 3]), positions_only: false },
        // Otzaria 7.0.1: a phrase word takes the sampled list plus the letter edits.
        Case { name: "typo in a phrase ברכה בראשית (Otzaria 7.0.1)", lines: &["ברכגה בראשית", "ברקה בראשית"],
            req: || SearchRequest { groups: vec![wo("ברכה", |o| o.spelling_errors = true), w("בראשית")], ..Default::default() },
            expect: Exactly(&[1, 2]), positions_only: false },
        // Otzaria advanced_typo_partial_single_word_finds_document.
        Case { name: "typo + part of word משה", lines: &["ויאמר משה אל העם", "ספר בראשית"],
            req: || SearchRequest { groups: vec![wo("משה", |o| { o.spelling_errors = true; o.part_of_word = true; })], ..Default::default() },
            expect: Exactly(&[1]), positions_only: false },
        // Otzaria typo_tolerance_scales_with_word_length (3 letters / 2 letters).
        Case { name: "typo שבת: confusions only", lines: &["שבת", "שבט", "סבת", "שבתי", "בשת", "שב"],
            req: || SearchRequest { groups: vec![wo("שבת", |o| o.spelling_errors = true)], ..Default::default() },
            expect: Exactly(&[1, 2, 3]), positions_only: false },
        Case { name: "typo אב: none", lines: &["אב", "עב", "אם", "אבג"],
            req: || SearchRequest { groups: vec![wo("אב", |o| o.spelling_errors = true)], ..Default::default() },
            expect: Exactly(&[1]), positions_only: false },
        // Otzaria grammatical_morphology_covers_verbs_and_stacked_prefixes and
        // grammatical_prefixes_need_a_root_and_skip_bare_alef.
        Case { name: "grammatical prefix slots אמר", lines: &["וכשאמר", "ושכשאמר", "אאמר", "דאמר", "דאכשהאמר"],
            req: || SearchRequest { groups: vec![wo("אמר", |o| o.grammar_prefixes = true)], ..Default::default() },
            expect: Exactly(&[1, 4, 5]), positions_only: false },
        Case { name: "grammatical prefix needs a root ית", lines: &["ית", "בית"],
            req: || SearchRequest { groups: vec![wo("ית", |o| o.grammar_prefixes = true)], ..Default::default() },
            expect: Exactly(&[1]), positions_only: false },
        Case { name: "custom spacing 0-1:2 1-2:0", lines: &["ויאמר ה' צבאות אל משה", "ויאמר אל העם ואל משה"],
            req: || spacing(vec![w("ויאמר"), w("אל"), w("משה")], vec![2, 0]),
            expect: Exactly(&[1]), positions_only: false },
        Case { name: "ordered ויאמר משה d2", lines: &["ויאמר להם משה", "אמר משה ויאמר"],
            req: || ordered(vec![w("ויאמר"), w("משה")], 2),
            expect: Exactly(&[1]), positions_only: false },
        Case { name: "any order אמר אמר d0 / repeated words", lines: ANY_ORDER,
            req: || any_order(vec![w("אמר"), w("אמר")], 0), expect: Exactly(&[]), positions_only: false },
        Case { name: "any order אמר אמר d1", lines: ANY_ORDER,
            req: || any_order(vec![w("אמר"), w("אמר")], 1), expect: Exactly(&[7]), positions_only: false },
        Case { name: "any order אמר רבי אמר d0", lines: ANY_ORDER,
            req: || any_order(vec![w("אמר"), w("רבי"), w("אמר")], 0), expect: Exactly(&[7]), positions_only: false },
        Case { name: "any order רבי אמר אמר d0", lines: ANY_ORDER,
            req: || any_order(vec![w("רבי"), w("אמר"), w("אמר")], 0), expect: Exactly(&[7]), positions_only: false },
        Case { name: "any order רבי אמר רבי d5", lines: ANY_ORDER,
            req: || any_order(vec![w("רבי"), w("אמר"), w("רבי")], 5), expect: Exactly(&[]), positions_only: false },
        Case { name: "any order להחזיר חייב d2 (query reversed)", lines: ANY_ORDER,
            req: || any_order(vec![w("להחזיר"), w("חייב")], 2), expect: Exactly(&[1, 2, 3, 5]), positions_only: false },
        Case { name: "any order חייב להחזיר d0", lines: ANY_ORDER,
            req: || any_order(vec![w("חייב"), w("להחזיר")], 0), expect: Exactly(&[1, 2]), positions_only: false },
        Case { name: "any order חייב להחזיר d2", lines: ANY_ORDER,
            req: || any_order(vec![w("חייב"), w("להחזיר")], 2), expect: Exactly(&[1, 2, 3, 5]), positions_only: false },
        Case { name: "any order חייב להחזיר d3", lines: ANY_ORDER,
            req: || any_order(vec![w("חייב"), w("להחזיר")], 3), expect: Exactly(&[1, 2, 3, 4, 5]), positions_only: false },
        Case { name: "ordered חייב להחזיר d0", lines: ANY_ORDER,
            req: || ordered(vec![w("חייב"), w("להחזיר")], 0), expect: Exactly(&[1]), positions_only: false },
        Case { name: "ordered חייב להחזיר d3", lines: ANY_ORDER,
            req: || ordered(vec![w("חייב"), w("להחזיר")], 3), expect: Exactly(&[1, 5]), positions_only: false },
        Case { name: "ordered להחזיר החפץ חייב d1", lines: ANY_ORDER,
            req: || ordered(vec![w("להחזיר"), w("החפץ"), w("חייב")], 1), expect: Exactly(&[3, 4]), positions_only: false },
        Case { name: "any order להחזיר החפץ חייב d1", lines: ANY_ORDER,
            req: || any_order(vec![w("להחזיר"), w("החפץ"), w("חייב")], 1), expect: Exactly(&[3, 4]), positions_only: false },
        Case { name: "any order חייב החפץ להחזיר d0", lines: ANY_ORDER,
            req: || any_order(vec![w("חייב"), w("החפץ"), w("להחזיר")], 0), expect: Exactly(&[]), positions_only: false },
        Case { name: "negative proximity: not חייב להחזיר adjacent in order", lines: ANY_ORDER,
            req: || SearchRequest { groups: vec![w("להחזיר")],
                exclude: Some(ExcludeSpec { groups: vec![w("חייב"), w("להחזיר")],
                    proximity: Some(ProximitySpec::Ordered { gaps: vec![0] }), ..Default::default() }), ..Default::default() },
            expect: Exactly(&[2, 3, 4, 5]), positions_only: false },
        Case { name: "negative proximity: not חייב להחזיר any order d1", lines: ANY_ORDER,
            req: || SearchRequest { groups: vec![w("להחזיר")],
                exclude: Some(ExcludeSpec { groups: vec![w("חייב"), w("להחזיר")],
                    proximity: Some(ProximitySpec::AnyOrder { distance: 1 }), ..Default::default() }), ..Default::default() },
            expect: Exactly(&[3, 4, 5]), positions_only: false },
        Case { name: "all words מתכוין בחלבים", lines: SCOPE,
            req: || SearchRequest { groups: vec![w("מתכוין"), w("בחלבים")], ..Default::default() },
            expect: Exactly(&[]), positions_only: false },
        Case { name: "anyWord מתכוין בחלבים", lines: SCOPE,
            req: || SearchRequest { groups: vec![w("מתכוין"), w("בחלבים")], min_groups: Some(1), ..Default::default() },
            expect: Exactly(&[1, 2, 4]), positions_only: false },
        Case { name: "mostWords מתכוין מתעסק בחלבים", lines: SCOPE,
            req: || SearchRequest { groups: vec![w("מתכוין"), w("מתעסק"), w("בחלבים")], min_groups: Some(2), ..Default::default() },
            expect: Exactly(&[2, 4]), positions_only: false },
        Case { name: "anyWord + negative בשבת", lines: SCOPE,
            req: || SearchRequest { groups: vec![w("מתכוין"), w("בחלבים")], min_groups: Some(1),
                exclude: Some(ExcludeSpec { groups: vec![w("בשבת")], ..Default::default() }), ..Default::default() },
            expect: Exactly(&[2, 4]), positions_only: false },
        Case { name: "exact הלך does not find הלכתי", lines: &["הלכתי", "מזרח"],
            req: || SearchRequest { groups: vec![w("הלך")], ..Default::default() },
            expect: Exactly(&[]), positions_only: false },
        // Otzaria's own test (hebrew_query.rs ~3704) asserts only מצוות ↛ מצת. מצות → מצת
        // IS generated by Otzaria's rule (the ו is an internal letter, 3 letters remain),
        // so the earlier "מצות never finds מצת" expectation was ours, not Otzaria's.
        Case { name: "ktiv מצות finds מצוות and itself", lines: &["מצוות", "מצות", "מצת"],
            req: || SearchRequest { groups: vec![wo("מצות", |o| o.ktiv_male_haser = true)], ..Default::default() },
            expect: Contains(&[1, 2], &[]), positions_only: false },
        Case { name: "ktiv מצוות does not find מצת", lines: &["מצוות", "מצות", "מצת"],
            req: || SearchRequest { groups: vec![wo("מצוות", |o| o.ktiv_male_haser = true)], ..Default::default() },
            expect: Contains(&[1], &[3]), positions_only: false },
        Case { name: "ktiv אותו does not find אות", lines: &["אות", "אותו"],
            req: || SearchRequest { groups: vec![wo("אותו", |o| o.ktiv_male_haser = true)], ..Default::default() },
            expect: Contains(&[2], &[1]), positions_only: false },
        Case { name: "grammatical suffixes תורה → תורתו", lines: &["תורתו", "מות", "אבות"],
            req: || SearchRequest { groups: vec![wo("תורה", |o| o.grammar_suffixes = true)], ..Default::default() },
            expect: Contains(&[1], &[]), positions_only: false },
        Case { name: "grammatical suffixes need 3 letters: מה ∌ מות", lines: &["תורתו", "מות", "אבות"],
            req: || SearchRequest { groups: vec![wo("מה", |o| o.grammar_suffixes = true)], ..Default::default() },
            expect: Contains(&[], &[2]), positions_only: false },
        Case { name: "grammatical suffixes need 3 letters: אב ∌ אבות", lines: &["תורתו", "מות", "אבות"],
            req: || SearchRequest { groups: vec![wo("אב", |o| o.grammar_suffixes = true)], ..Default::default() },
            expect: Contains(&[], &[3]), positions_only: false },
        Case { name: "acronym phrase alternative רמב\"ם ↔ רבי משה בן מיימון",
            lines: &["אמר רמב\"ם בהלכות", "כתב רבי משה בן מיימון בספרו", "דבר אחר לגמרי"],
            req: || SearchRequest { groups: vec![w("רמב\"ם")],
                phrase_alternatives: vec![vec![w("רבי"), w("משה"), w("בן"), w("מיימון")]], ..Default::default() },
            expect: Exactly(&[1, 2]), positions_only: false },
        // ---- Otzaria 7.0.1 (otzaria_search_engine 5babbce, issue #36),
        // search_quality_tests.rs; an Otzaria phrase = ordered, distance 0.
        // phrase_typo_takes_any_single_letter_edit
        Case { name: "7.0.1 phrase typo: substitution, first word", lines: PALM,
            req: || ordered(vec![wo(LLBVT, typo), w(DQL)], 0), expect: Contains(&[1], &[]), positions_only: false },
        Case { name: "7.0.1 phrase typo: substitution, second word", lines: PALM,
            req: || ordered(vec![w(PALMITO), wo(LLBVT, typo)], 0), expect: Exactly(&[1]), positions_only: false },
        Case { name: "7.0.1 phrase typo: insertion", lines: PALM,
            req: || ordered(vec![wo("\u{05DC}\u{05D1}\u{05D1}\u{05D1}\u{05D5}\u{05EA}", typo), w(DQL)], 0), expect: Contains(&[1], &[]), positions_only: false },
        Case { name: "7.0.1 phrase typo: deletion", lines: PALM,
            req: || ordered(vec![wo("\u{05DC}\u{05D1}\u{05D5}\u{05EA}", typo), w(DQL)], 0), expect: Contains(&[1], &[]), positions_only: false },
        Case { name: "7.0.1 phrase typo: transposition", lines: PALM,
            req: || ordered(vec![wo("\u{05DC}\u{05D1}\u{05D1}\u{05EA}\u{05D5}", typo), w(DQL)], 0), expect: Contains(&[1], &[]), positions_only: false },
        Case { name: "7.0.1 phrase typo + grammatical prefixes", lines: PALM,
            req: || ordered(vec![wo(LLBVT, typo_gram), wo(DQL, typo_gram)], 0), expect: Contains(&[1], &[]), positions_only: false },
        Case { name: "7.0.1 phrase typo: 3 letters confusions only", lines: PALM,
            req: || ordered(vec![wo("\u{05DC}\u{05D1}\u{05D1}\u{05D5}\u{05EA}", typo), wo("\u{05D3}\u{05E7}\u{05E8}", typo)], 0), expect: Exactly(&[]), positions_only: false },
        // phrase_typo_edits_keep_every_sampled_variant
        Case { name: "7.0.1 phrase typo edits keep the sampled variants", lines: &["\u{05E1}\u{05E4}\u{05E8}\u{05DD} \u{05E9}\u{05DC}\u{05D5}\u{05DD}"],
            req: || ordered(vec![wo("\u{05E1}\u{05E4}\u{05E8}\u{05D9}\u{05DD}", |o| {
                o.spelling_errors = true; o.grammar_prefixes = true; o.grammar_suffixes = true; o.aramaic_prefixes = true; o.aramaic_suffixes = true;
            }), w(SHALOM)], 0), expect: Exactly(&[1]), positions_only: false },
        // phrase_typo_composes_with_spelling
        Case { name: "7.0.1 phrase typo composes with ktiv", lines: &["\u{05D1}\u{05D9}\u{05E8}\u{05E7}\u{05D4} \u{05E9}\u{05DC}\u{05D5}\u{05DD}", "\u{05D1}\u{05E8}\u{05D9}\u{05E7}\u{05D4} \u{05E9}\u{05DC}\u{05D5}\u{05DD}", "\u{05D1}\u{05D5}\u{05E8}\u{05E7}\u{05D4} \u{05E9}\u{05DC}\u{05D5}\u{05DD}"],
            req: || ordered(vec![wo("\u{05D1}\u{05E8}\u{05DB}\u{05D4}", |o| { o.spelling_errors = true; o.ktiv_male_haser = true; }), w(SHALOM)], 0),
            expect: Exactly(&[1, 2, 3]), positions_only: false },
        // final_letter_suffix_windows_keep_phrase_typo_results (ccefc81)
        Case { name: "7.0.1 final letter + suffix window keeps phrase typos", lines: &["\u{05D0}\u{05D1}\u{05E8}\u{05E2}\u{05DD} \u{05E9}\u{05DC}\u{05D5}\u{05DD}", "\u{05D5}\u{05D0}\u{05D1}\u{05E8}\u{05E2}\u{05DD} \u{05E9}\u{05DC}\u{05D5}\u{05DD}", "\u{05D0}\u{05D1}\u{05E8}\u{05D4}\u{05DD} \u{05E9}\u{05DC}\u{05D5}\u{05DD}"],
            req: || ordered(vec![wo("\u{05D0}\u{05D1}\u{05E8}\u{05D4}\u{05DD}", |o| {
                o.spelling_errors = true; o.ktiv_male_haser = true; o.advanced_suffixes = true; o.aramaic_prefixes = true;
            }), w(SHALOM)], 0), expect: Exactly(&[1, 2, 3]), positions_only: false },
        // hebrew_query.rs aramaic_prefix_composes_with_the_other_options (bf684bc)
        Case { name: "7.0.1 Aramaic prefixes compose with part of word + grammatical suffixes",
            lines: &["\u{05D0}\u{05E9}\u{05D1}\u{05EA}\u{05D0}", "\u{05D3}\u{05E9}\u{05D1}\u{05EA}\u{05D5}\u{05EA}", "\u{05D3}\u{05E9}\u{05D1}\u{05EA}\u{05D0}\u{05D9}", "\u{05E9}\u{05D1}\u{05EA}\u{05D5}\u{05EA}\u{05D9}\u{05D4}\u{05DD}"],
            req: || SearchRequest { groups: vec![wo("\u{05E9}\u{05D1}\u{05EA}", |o| { o.aramaic_prefixes = true; o.part_of_word = true; o.grammar_suffixes = true; })], ..Default::default() },
            expect: Exactly(&[1, 2, 3, 4]), positions_only: false },
        Case { name: "7.0.1 Aramaic + grammatical prefixes: no root floor", lines: &["\u{05D3}\u{05D0}\u{05D1}"],
            req: || SearchRequest { groups: vec![wo("\u{05D0}\u{05D1}", |o| { o.aramaic_prefixes = true; o.grammar_prefixes = true; })], ..Default::default() },
            expect: Exactly(&[1]), positions_only: false },
        Case { name: "7.0.1 Aramaic prefixes + suffix window on a short root", lines: &["\u{05D5}\u{05D0}\u{05D1}\u{05D5}\u{05EA}"],
            req: || SearchRequest { groups: vec![wo("\u{05D0}\u{05D1}", |o| { o.aramaic_prefixes = true; o.grammar_prefixes = true; o.advanced_suffixes = true; })], ..Default::default() },
            expect: Exactly(&[1]), positions_only: false },
    ]
}

#[test]
fn otzaria_cases() {
    let mut failures = Vec::new();
    let mut ran = 0;
    for c in cases() {
        for positions in [true, false] {
            if c.positions_only && !positions {
                continue;
            }
            ran += 1;
            let got = matches(c.lines, positions, &(c.req)());
            let ok = match &c.expect {
                Expect::Exactly(e) => got == *e,
                Expect::Contains(must, never) => {
                    must.iter().all(|x| got.contains(x)) && never.iter().all(|x| !got.contains(x))
                }
            };
            if !ok {
                let want = match &c.expect {
                    Expect::Exactly(e) => format!("exactly {e:?}"),
                    Expect::Contains(m, n) => format!("contains {m:?}, never {n:?}"),
                };
                failures.push(format!("{} [positions={positions}]: got {got:?}, Otzaria expects {want}", c.name));
            }
        }
    }
    for f in &failures {
        eprintln!("FAIL {f}");
    }
    eprintln!("{} of {ran} case runs pass", ran - failures.len());
    assert!(failures.is_empty(), "{} Otzaria case runs fail (see FAIL lines)", failures.len());
}

/// Otzaria's per-word options (`wordOptions` / `alternativeWords`) through
/// the public API: global options with one word overriding them, and an
/// alternative word for a slot. Same lines as the hand-built slots.
#[test]
fn per_word_overrides_through_search_options() {
    let lines: &[&str] = &["הספר מלכא", "ספר דמלכא", "הספר דמלכא", "ספר שר", "מטבע מלך"];
    let mut options = SearchOptions {
        query: "ספר מלכה".into(),
        grammar_prefixes: true, // every word...
        ..Default::default()
    };
    options.words.insert(
        1,
        WordOptions {
            grammar_prefixes: Some(false), // ...but word 1: Aramaic affixes instead
            aramaic_prefixes: Some(true),
            aramaic_suffixes: Some(true),
            ..Default::default()
        },
    );
    let by_hand = SearchRequest {
        groups: vec![
            wo("ספר", |o| o.grammar_prefixes = true),
            wo("מלכה", |o| {
                o.aramaic_prefixes = true;
                o.aramaic_suffixes = true;
            }),
        ],
        ..Default::default()
    };
    // alternatives: word 1 of "ספר שר" may also be מלך (its slot ORs them)
    let mut alts = SearchOptions { query: "מטבע שר".into(), ..Default::default() };
    alts.words.insert(1, WordOptions { alternatives: vec!["מלך".into()], ..Default::default() });
    for positions in [true, false] {
        let got = matches_options(lines, positions, &options);
        assert_eq!(got, vec![1, 2, 3], "positions={positions}");
        assert_eq!(got, matches(lines, positions, &by_hand), "positions={positions}");
        assert_eq!(matches_options(lines, positions, &alts), vec![5], "positions={positions}");
        // without the override, word 1 has only grammar prefixes: no Aramaic forms
        options.words.clear();
        assert!(matches_options(lines, positions, &options).is_empty(), "positions={positions}");
        options.words.insert(
            1,
            WordOptions {
                grammar_prefixes: Some(false),
                aramaic_prefixes: Some(true),
                aramaic_suffixes: Some(true),
                ..Default::default()
            },
        );
    }
}

/// Otzaria's negative per-word options (`negative_search_options`, keyed by
/// word index; the negative query is built by the positive's builder): a
/// negative word with its own grammatical prefixes, its own alternative
/// (`negative_alternative_words`) and the acronym phrase of the negative
/// query, through the public options API.
#[test]
fn negative_word_has_its_own_options() {
    let amar = "\u{05D0}\u{05DE}\u{05E8}";
    let rabbi = "\u{05E8}\u{05D1}\u{05D9}";
    let meir = "\u{05DE}\u{05D0}\u{05D9}\u{05E8}";
    let lines: &[&str] = &[
        "\u{05D0}\u{05DE}\u{05E8} \u{05E8}\u{05D1}\u{05D9} \u{05D9}\u{05D5}\u{05D7}\u{05E0}\u{05DF}",
        "\u{05D5}\u{05D0}\u{05DE}\u{05E8} \u{05E8}\u{05D1}\u{05D9} \u{05D9}\u{05D5}\u{05D7}\u{05E0}\u{05DF}",
        "\u{05E9}\u{05D0}\u{05DE}\u{05E8} \u{05E8}\u{05D1}\u{05D9} \u{05DE}\u{05D0}\u{05D9}\u{05E8}",
        "\u{05E8}\u{05D1}\u{05D9} \u{05DE}\u{05D0}\u{05D9}\u{05E8}",
    ];
    let search = |global_prefixes: bool, word: Option<WordOptions>| {
        let mut ex = ExcludeOptions { query: amar.into(), ..Default::default() };
        if let Some(w) = word {
            ex.words.insert(0, w);
        }
        SearchOptions { query: rabbi.into(), grammar_prefixes: global_prefixes, exclude: Some(ex), ..Default::default() }
    };
    let prefixes = |on: bool| Some(WordOptions { grammar_prefixes: Some(on), ..Default::default() });
    for positions in [true, false] {
        // the negative word as typed
        assert_eq!(matches_options(lines, positions, &search(false, None)), vec![2, 3, 4], "positions={positions}");
        // its own grammatical prefixes: also the prefixed forms
        assert_eq!(matches_options(lines, positions, &search(false, prefixes(true))), vec![4], "positions={positions}");
        // the global options are only its default: off for it alone
        assert_eq!(matches_options(lines, positions, &search(true, None)), vec![4], "positions={positions}");
        assert_eq!(matches_options(lines, positions, &search(true, prefixes(false))), vec![2, 3, 4], "positions={positions}");
        // its own alternative word joins its slot
        let alt = Some(WordOptions { alternatives: vec![meir.into()], ..Default::default() });
        assert_eq!(matches_options(lines, positions, &search(false, alt)), vec![2], "positions={positions}");
    }
    // the negative query's acronym phrase (Otzaria ORs it into the negative)
    let lines: &[&str] = &[
        "\u{05DB}\u{05EA}\u{05D1} \u{05E8}\u{05DE}\u{05D1}\"\u{05DD} \u{05D1}\u{05D4}\u{05DC}\u{05DB}\u{05D5}\u{05EA}",
        "\u{05DB}\u{05EA}\u{05D1} \u{05E8}\u{05D1}\u{05D9} \u{05DE}\u{05E9}\u{05D4} \u{05D1}\u{05DF} \u{05DE}\u{05D9}\u{05D9}\u{05DE}\u{05D5}\u{05DF} \u{05D1}\u{05E1}\u{05E4}\u{05E8}\u{05D5}",
        "\u{05DB}\u{05EA}\u{05D1} \u{05E8}\u{05D1}\u{05D9} \u{05DE}\u{05E9}\u{05D4} \u{05D1}\u{05E1}\u{05E4}\u{05E8}\u{05D5}",
        "\u{05DB}\u{05EA}\u{05D1} \u{05DE}\u{05E9}\u{05D4} \u{05D1}\u{05DF} \u{05E8}\u{05D1}\u{05D9} \u{05DE}\u{05D9}\u{05D9}\u{05DE}\u{05D5}\u{05DF}",
    ];
    let o = SearchOptions {
        query: "\u{05DB}\u{05EA}\u{05D1}".into(),
        exclude: Some(ExcludeOptions {
            query: "\u{05E8}\u{05DE}\u{05D1}\"\u{05DD}".into(),
            phrase_alternatives: vec![PhraseOptions {
                query: "\u{05E8}\u{05D1}\u{05D9} \u{05DE}\u{05E9}\u{05D4} \u{05D1}\u{05DF} \u{05DE}\u{05D9}\u{05D9}\u{05DE}\u{05D5}\u{05DF}".into(),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    for positions in [true, false] {
        assert_eq!(matches_options(lines, positions, &o), vec![3, 4], "positions={positions}");
    }
}

/// The rule evaluated on positions (span query) and validated on the text
/// (no positions) gives the same lines AND the same snippet windows, on
/// lines built to hit the edges: repeated words, one word filling two
/// query words, reversed order, gaps exactly at the limit and one past it.
#[test]
fn proximity_positions_equal_text_validation() {
    const LINES: &[&str] = &[
        "אב גד",
        "גד אב",
        "אב קק גד",
        "אב קק קק גד",
        "אב קק קק קק גד",
        "גד קק קק אב",
        "אב אב גד",
        "אב גד אב",
        "גד אב גד אב",
        "אב קק גד קק הו",
        "הו קק גד קק אב",
        "אב קק קק קק קק אב גד הו",
        "הו גד אב",
        "אב קק הו קק קק גד",
        "גד הו קק קק קק קק אב",
        "אב",
        "קק קק קק",
    ];
    let ab = || vec![w("אב"), w("גד")];
    let abh = || vec![w("אב"), w("גד"), w("הו")];
    let either = || GroupSpec::Any { alternatives: vec![w("אב"), w("גד")] };
    let mut requests = Vec::new();
    for d in 0..4 {
        requests.push(ordered(ab(), d));
        requests.push(any_order(ab(), d));
        requests.push(ordered(abh(), d));
        requests.push(any_order(abh(), d));
        requests.push(any_order(vec![w("אב"), w("אב")], d));
        requests.push(ordered(vec![w("אב"), w("אב")], d));
        requests.push(any_order(vec![either(), w("אב")], d));
        requests.push(ordered(vec![either(), w("גד")], d));
    }
    requests.push(spacing(abh(), vec![0, 2]));
    requests.push(spacing(abh(), vec![2, 0]));
    requests.push(spacing(abh(), vec![3, 1]));
    let mut nonempty = 0;
    for req in &requests {
        let with = located(LINES, true, req);
        let without = located(LINES, false, req);
        assert_eq!(with, without, "{:?}", req.proximity);
        nonempty += usize::from(!with.is_empty());
    }
    assert!(nonempty > requests.len() / 2);
    // spot checks against the rule itself
    assert_eq!(matches(LINES, true, &ordered(ab(), 2)), [1, 3, 4, 7, 8, 9, 10, 12]);
    assert_eq!(matches(LINES, false, &any_order(ab(), 2)), [1, 2, 3, 4, 6, 7, 8, 9, 10, 11, 12, 13]);
}

/// One-letter words are words (Otzaria): `a x b` checks `x` at its own
/// position, a wrong one-letter word does not match, and distances count
/// it, on both index flavours.
#[test]
fn one_letter_word_is_matched_at_its_position() {
    let lines: &[&str] = &[
        "משה ו תורה",
        "משה אמר ו תורה",
        "משה אמר לו ו תורה",
        "תורה ו משה",
        "משה ה תורה", // wrong one-letter word
        "משה תורה",   // no one-letter word
    ];
    let mut o = SearchOptions { query: "משה ו תורה".into(), ..Default::default() };
    for positions in [true, false] {
        o.proximity = Some(ProximitySpec::Ordered { gaps: vec![1, 1] });
        assert_eq!(matches_options(lines, positions, &o), vec![1, 2], "ordered positions={positions}");
        o.proximity = Some(ProximitySpec::AnyOrder { distance: 0 });
        assert_eq!(matches_options(lines, positions, &o), vec![1, 4], "any order positions={positions}");
        // no distance rule: still every word, ו included
        o.proximity = None;
        assert_eq!(matches_options(lines, positions, &o), vec![1, 2, 3, 4], "AND positions={positions}");
    }
}

/// A typed word with no letters or digits is dropped and its gaps merge;
/// the one-letter words of the text count in the distance.
#[test]
fn no_letter_word_merges_its_gaps() {
    let lines: &[&str] = &["משה תורה", "משה ו תורה", "משה אמר ו תורה"];
    let o = SearchOptions {
        query: "משה , תורה".into(),
        proximity: Some(ProximitySpec::Ordered { gaps: vec![0, 1] }),
        ..Default::default()
    };
    for positions in [true, false] {
        assert_eq!(matches_options(lines, positions, &o), vec![1, 2], "positions={positions}");
    }
}

/// One-letter words count in FtsLib's word distance too, and expansions
/// reach one-letter index terms.
#[test]
fn one_letter_words_count_and_expand() {
    let lines: &[&str] = &["משה ו תורה", "א ג", "משה אמר תורה"];
    let near = SearchOptions { query: "משה תורה".into(), max_word_distance: Some(0), ..Default::default() };
    let fuzzy = SearchOptions { query: "אב~1".into(), ..Default::default() };
    for positions in [true, false] {
        assert!(matches_options(lines, positions, &near).is_empty(), "positions={positions}");
        assert_eq!(matches_options(lines, positions, &fuzzy), vec![2], "positions={positions}");
    }
}

/// Digit runs are words (Otzaria's tokenizer): searched, matched at their
/// position, and counted in distances; digits join the letters around them.
#[test]
fn digit_runs_are_words() {
    let lines: &[&str] = &[
        "סימן 12 סעיף ב",
        "סימן ב5 סעיף",
        "סימן סעיף",
        "סימן 3 4 סעיף",
    ];
    let q = |query: &str, distance: Option<usize>| SearchOptions {
        query: query.into(),
        max_word_distance: distance,
        ..Default::default()
    };
    for positions in [true, false] {
        assert_eq!(matches_options(lines, positions, &q("12", None)), vec![1], "positions={positions}");
        assert_eq!(matches_options(lines, positions, &q("ב5", None)), vec![2], "positions={positions}");
        assert_eq!(matches_options(lines, positions, &q("סימן סעיף", Some(0))), vec![3], "positions={positions}");
        assert_eq!(matches_options(lines, positions, &q("סימן סעיף", Some(1))), vec![1, 2, 3], "positions={positions}");
        let ordered = SearchOptions {
            query: "סימן 12 סעיף".into(),
            proximity: Some(ProximitySpec::Ordered { gaps: vec![0, 0] }),
            ..Default::default()
        };
        assert_eq!(matches_options(lines, positions, &ordered), vec![1], "ordered positions={positions}");
    }
}

/// Otzaria's option setters by name (its checkbox labels).
const GRAM_PREFIX: fn(&mut WalkOptionsSpec) = |o| o.grammar_prefixes = true;
const GRAM_SUFFIX: fn(&mut WalkOptionsSpec) = |o| o.grammar_suffixes = true;
const KTIV: fn(&mut WalkOptionsSpec) = |o| o.ktiv_male_haser = true;
const PART: fn(&mut WalkOptionsSpec) = |o| o.part_of_word = true;
const TYPO: fn(&mut WalkOptionsSpec) = |o| o.spelling_errors = true;
const ARAMAIC_PREFIX: fn(&mut WalkOptionsSpec) = |o| o.aramaic_prefixes = true;
const ARAMAIC_SUFFIX: fn(&mut WalkOptionsSpec) = |o| o.aramaic_suffixes = true;
const PREFIX: fn(&mut WalkOptionsSpec) = |o| o.advanced_prefixes = true;
const SUFFIX: fn(&mut WalkOptionsSpec) = |o| o.advanced_suffixes = true;

/// An Otzaria phrase (ordered, distance 0) with every word under `per_word`.
fn phrase_with(words: &[&str], per_word: &[&[fn(&mut WalkOptionsSpec)]]) -> SearchRequest {
    let groups = words
        .iter()
        .zip(per_word)
        .map(|(word, setters)| {
            let mut o = WalkOptionsSpec::default();
            for set in setters.iter() {
                set(&mut o);
            }
            GroupSpec::Word {
                word: (*word).into(),
                options: o,
            }
        })
        .collect();
    ordered(groups, 0)
}

fn split(query: &str) -> Vec<&str> {
    query.split(' ').collect()
}

/// Otzaria 7.0.1 `aramaic_prefixes_never_drop_matches_of_the_other_options`
/// (otzaria#3673, bf684bc): adding Aramaic prefixes to any set of the
/// regular options never drops a match that set found, typo variants of a
/// phrase word included.
#[test]
fn aramaic_prefixes_never_drop_matches_of_the_other_options() {
    let lines: &[&str] = &[
        "\u{05DC}\u{05D1}\u{05D1}\u{05D5}\u{05EA} \u{05D3}\u{05E7}\u{05DC}",
        "\u{05D5}\u{05DC}\u{05D1}\u{05D1}\u{05D5}\u{05EA} \u{05D3}\u{05E7}\u{05DC}",
        "\u{05D3}\u{05DC}\u{05D1}\u{05D1}\u{05D5}\u{05EA} \u{05D3}\u{05E7}\u{05DC}",
        "\u{05DC}\u{05D1}\u{05D5}\u{05D5}\u{05EA} \u{05D3}\u{05E7}\u{05DC}",
        "\u{05DC}\u{05DC}\u{05D1}\u{05D1}\u{05EA} \u{05D3}\u{05E7}\u{05DC}",
        "\u{05D3}\u{05E7}\u{05DC} \u{05DC}\u{05DC}\u{05D1}\u{05D1}\u{05EA}",
        "\u{05D3}\u{05E7}\u{05DC} \u{05D3}\u{05DC}\u{05DC}\u{05D1}\u{05D5}\u{05EA}",
    ];
    let regular = [GRAM_PREFIX, GRAM_SUFFIX, KTIV, PART, TYPO];
    let mut bases: Vec<Vec<fn(&mut WalkOptionsSpec)>> = vec![Vec::new(), regular.to_vec()];
    bases.extend(regular.iter().map(|o| vec![*o]));
    for query in [
        "\u{05DC}\u{05D1}\u{05D1}\u{05D5}\u{05EA} \u{05D3}\u{05E7}\u{05DC}",
        "\u{05DC}\u{05DC}\u{05D1}\u{05D5}\u{05EA} \u{05D3}\u{05E7}\u{05DC}",
        "\u{05D3}\u{05E7}\u{05DC} \u{05DC}\u{05DC}\u{05D1}\u{05D5}\u{05EA}",
    ] {
        let words = split(query);
        for positions in [true, false] {
            for (b, base) in bases.iter().enumerate() {
                let before = matches(lines, positions, &phrase_with(&words, &[base, base]));
                let mut with = base.clone();
                with.push(ARAMAIC_PREFIX);
                let after = matches(lines, positions, &phrase_with(&words, &[&with, &with]));
                let lost: Vec<usize> = before
                    .iter()
                    .filter(|l| !after.contains(l))
                    .copied()
                    .collect();
                assert!(
                    lost.is_empty(),
                    "query {query:?} base #{b} positions={positions}: lost {lost:?}"
                );
            }
        }
    }
    let mut all = regular.to_vec();
    all.push(ARAMAIC_PREFIX);
    let words = split("\u{05DC}\u{05D1}\u{05D1}\u{05D5}\u{05EA} \u{05D3}\u{05E7}\u{05DC}");
    for positions in [true, false] {
        let got = matches(lines, positions, &phrase_with(&words, &[&all, &all]));
        for line in [1, 2, 3] {
            assert!(got.contains(&line), "line {line} missing from {got:?}");
        }
    }
}

/// Otzaria 7.0.1 `phrase_typo_edit_survives_the_other_options`
/// (otzaria#3673): a phrase word's single-letter edit survives each option
/// that composes with typo tolerance, and all of them.
#[test]
fn phrase_typo_edit_survives_the_other_options() {
    let lines: &[&str] = &[PALM[0]];
    let extra = [
        GRAM_PREFIX,
        GRAM_SUFFIX,
        KTIV,
        ARAMAIC_PREFIX,
        ARAMAIC_SUFFIX,
        PART,
    ];
    let all = (1u32 << extra.len()) - 1;
    for mask in (0..extra.len())
        .map(|i| 1u32 << i)
        .chain([all & !(1 << 5), all])
    {
        let mut word: Vec<fn(&mut WalkOptionsSpec)> = vec![TYPO];
        word.extend(
            (0..extra.len())
                .filter(|i| mask >> i & 1 == 1)
                .map(|i| extra[i]),
        );
        for positions in [true, false] {
            let got = matches(lines, positions, &phrase_with(&[LLBVT, DQL], &[&word, &[]]));
            assert_eq!(got, [1], "mask {mask:#b} positions={positions}");
        }
    }
}

/// Otzaria 7.0.1 `collect_ranked` (653781c, its test
/// `phrase_typo_scan_cap_keeps_the_exact_match`): when a typo word's
/// expansion overflows the cap, the forms the other options match without a
/// typo keep their slots before the typo edits, though the edits are shorter.
#[test]
fn typo_overflow_keeps_the_exact_option_forms_first() {
    // the typed word with a grammatical prefix, and many shorter edits
    let word = "\u{05DC}\u{05D1}\u{05D5}\u{05EA}";
    let prefixed = "\u{05D5}\u{05DC}\u{05D1}\u{05D5}\u{05EA} \u{05E9}\u{05DC}\u{05D5}\u{05DD}";
    let mut owned: Vec<String> = vec![prefixed.to_string()];
    let letters = [
        '\u{05D0}', '\u{05D2}', '\u{05D3}', '\u{05D4}', '\u{05D6}', '\u{05D7}', '\u{05D8}',
        '\u{05DB}', '\u{05DE}', '\u{05E0}', '\u{05E1}', '\u{05E2}', '\u{05E4}', '\u{05E6}',
        '\u{05E7}', '\u{05E8}',
    ];
    let chars: Vec<char> = word.chars().collect();
    for i in 0..chars.len() {
        for &c in &letters {
            let mut edited = chars.clone();
            edited[i] = c;
            owned.push(edited.into_iter().collect());
        }
    }
    let lines: Vec<&str> = owned.iter().map(String::as_str).collect();
    for positions in [true, false] {
        let req = SearchRequest {
            groups: vec![wo(word, |o| {
                o.spelling_errors = true;
                o.grammar_prefixes = true;
            })],
            max_expanded_terms: 3,
            ..Default::default()
        };
        let got = matches(&lines, positions, &req);
        assert!(got.contains(&1), "positions={positions}: {got:?}");
        assert_eq!(got.len(), 3, "positions={positions}: {got:?}");
    }
}

/// Lines for the overflow tests: the typed word with a grammatical prefix
/// (an exact option form), the typed word itself when `typed`, and many
/// substitutions of the typed word, shorter than the prefixed form.
fn overflow_lines(typed: bool) -> Vec<String> {
    let word = "\u{05DC}\u{05D1}\u{05D5}\u{05EA}";
    let mut lines = vec![
        "\u{05D5}\u{05DC}\u{05D1}\u{05D5}\u{05EA} \u{05E9}\u{05DC}\u{05D5}\u{05DD}".to_string(),
    ];
    if typed {
        lines.push(word.to_string());
    }
    let letters = [
        '\u{05D0}', '\u{05D2}', '\u{05D3}', '\u{05D4}', '\u{05D6}', '\u{05D7}', '\u{05D8}',
        '\u{05DB}', '\u{05DE}', '\u{05E0}', '\u{05E1}', '\u{05E2}', '\u{05E4}', '\u{05E6}',
        '\u{05E7}', '\u{05E8}',
    ];
    let chars: Vec<char> = word.chars().collect();
    for i in 0..chars.len() {
        for &c in &letters {
            let mut edited = chars.clone();
            edited[i] = c;
            lines.push(edited.into_iter().collect());
        }
    }
    lines
}

fn overflow_request(cap: usize) -> SearchRequest {
    SearchRequest {
        groups: vec![wo("\u{05DC}\u{05D1}\u{05D5}\u{05EA}", |o| {
            o.spelling_errors = true;
            o.grammar_prefixes = true;
        })],
        max_expanded_terms: cap,
        ..Default::default()
    }
}

/// Matching lines on an index written in `batches` commits (one segment
/// each), and the number of segments.
fn matches_in_segments(lines: &[&str], batches: usize, req: &SearchRequest) -> (Vec<usize>, usize) {
    let dir = tempfile::tempdir().unwrap();
    let index = LineIndex::create(
        dir.path(),
        IndexOptions {
            positions: true,
            keep_marks: false,
        },
    )
    .unwrap();
    let mut wr = index.writer(30_000_000, 1).unwrap();
    let per = lines.len().div_ceil(batches);
    for (i, t) in lines.iter().enumerate() {
        wr.add(&LineDoc {
            line_id: i as u64 + 1,
            position: i as u64,
            book_id: 1,
            text: (*t).into(),
            section_id: None,
            no_cross: false,
        })
        .unwrap();
        if (i + 1) % per == 0 {
            wr.commit(None).unwrap();
        }
    }
    wr.commit(None).unwrap();
    let segments = index.reader().unwrap().searcher().segment_readers().len();
    let r = index.search(req).unwrap();
    let mut got: Vec<usize> = r.hits.iter().map(|h| h.line_id as usize).collect();
    got.sort();
    (got, segments)
}

/// The overflow rank with the pinned exact word: the typed word keeps its
/// pinned slot, the exact option form its rank-0 slot, and the edits get
/// the rest of the cap.
#[test]
fn typo_overflow_rank_keeps_the_pinned_typed_word() {
    let owned = overflow_lines(true);
    let lines: Vec<&str> = owned.iter().map(String::as_str).collect();
    for positions in [true, false] {
        let got = matches(&lines, positions, &overflow_request(3));
        assert!(
            got.contains(&1) && got.contains(&2),
            "positions={positions}: {got:?}"
        );
        assert_eq!(got.len(), 3, "positions={positions}: {got:?}");
    }
}

/// The overflow rank is one cap across segments: an index split into
/// several segments keeps exactly the lines one segment keeps.
#[test]
fn typo_overflow_rank_is_one_cap_across_segments() {
    for typed in [false, true] {
        let owned = overflow_lines(typed);
        let lines: Vec<&str> = owned.iter().map(String::as_str).collect();
        let (one, n1) = matches_in_segments(&lines, 1, &overflow_request(3));
        let (many, n) = matches_in_segments(&lines, 4, &overflow_request(3));
        assert_eq!(n1, 1);
        assert!(n > 1, "{n} segments");
        assert!(one.contains(&1), "{one:?}");
        assert_eq!(one.len(), 3, "{one:?}");
        assert_eq!(many, one, "typed={typed}");
    }
}

/// Otzaria 7.0.1 `build_word_regex`: the typo edits run over every
/// candidate, the user's alternative words included. Here an alternative
/// is its own walk with the word's options, so it takes the edits too.
#[test]
fn alternative_words_take_the_typo_edits() {
    let query = format!("\u{05E1}\u{05E4}\u{05E8}\u{05D9}\u{05DD} {DQL}");
    let mut with_alt = SearchOptions {
        query: query.clone(),
        spelling_errors: true,
        ..Default::default()
    };
    // edits of the alternative: a substitution (line 1), a deletion (line 2)
    with_alt.words.insert(
        0,
        WordOptions {
            alternatives: vec![LLBVT.into()],
            ..Default::default()
        },
    );
    let without = SearchOptions {
        query,
        spelling_errors: true,
        ..Default::default()
    };
    for positions in [true, false] {
        assert_eq!(
            matches_options(PALM, positions, &with_alt),
            vec![1, 2],
            "positions={positions}"
        );
        assert!(
            matches_options(PALM, positions, &without).is_empty(),
            "positions={positions}"
        );
    }
}
