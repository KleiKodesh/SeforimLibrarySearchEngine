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
    ExcludeSpec, GroupSpec, IndexOptions, LineDoc, LineIndex, ProximitySpec, SearchOptions, SearchRequest,
    SearchResult, TypoModeSpec, WalkOptionsSpec, WordOptions,
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
        wr.add(&LineDoc { line_id: i as u64 + 1, position: i as u64, book_id: 1, text: (*t).into() })
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
        // TypoMode::Otzaria: with another option, Otzaria's sampled list
        // (no ג insertion); FullEdit1: the full edit distance 1.
        Case { name: "typo + grammatical prefixes ברכה (Otzaria list)", lines: &["ברכה", "ברכגה", "והברקה", "תורה"],
            req: || SearchRequest { groups: vec![wo("ברכה", |o| { o.spelling_errors = true; o.grammar_prefixes = true; })], ..Default::default() },
            expect: Exactly(&[1, 3]), positions_only: false },
        Case { name: "typo + grammatical prefixes ברכה (FullEdit1)", lines: &["ברכה", "ברכגה", "והברקה", "תורה"],
            req: || SearchRequest { groups: vec![wo("ברכה", |o| { o.spelling_errors = true; o.grammar_prefixes = true; o.typo_mode = TypoModeSpec::FullEdit1; })], ..Default::default() },
            expect: Exactly(&[1, 2, 3]), positions_only: false },
        // Otzaria: a phrase word takes the sampled list, not the scan.
        Case { name: "typo in a phrase ברכה בראשית (Otzaria list)", lines: &["ברכגה בראשית", "ברקה בראשית"],
            req: || SearchRequest { groups: vec![wo("ברכה", |o| o.spelling_errors = true), w("בראשית")], ..Default::default() },
            expect: Exactly(&[2]), positions_only: false },
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
