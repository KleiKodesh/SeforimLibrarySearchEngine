//! Golden tests: the Otzaria helpers (`hebrew_fts::otzaria`) against the output of
//! otzaria_search_engine 7.2.0 on the same inputs. `fixtures/otzaria_helpers/golden.json`
//! was written by calling the old engine's Dart API (`sanitizeQuery`, `splitQueryWords`,
//! `queryWordSpans`, `generateLiteralHighlightPattern`, `generateSmartHighlightPattern`,
//! `generateHighlightPattern` + its matcher, `parseUserRegex`, `prepareUserRegexMatcher`)
//! with `inputs.json` (the Hebrew is written there as `\u` escapes).

use std::collections::HashMap;

use hebrew_fts::otzaria::{self, HighlightMatch, HighlightPattern};
use hebrew_fts::user_regex::{self, UserRegexKind};
use serde_json::{json, Value};

fn load(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/otzaria_helpers/{name}", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn matches(ms: Vec<HighlightMatch>) -> Value {
    Value::Array(
        ms.into_iter()
            .map(|m| json!([m.start, m.end, m.word_ranges.iter().map(|r| json!([r.start, r.end])).collect::<Vec<_>>()]))
            .collect(),
    )
}

fn pattern(p: Option<HighlightPattern>, texts: &[String]) -> Value {
    let Some(p) = p else { return Value::Null };
    let m = &p.matcher;
    json!({
        "combined": p.combined_pattern,
        "words": p.word_patterns,
        "eligible": p.word_boundary_eligible,
        "crossLine": m.allows_cross_line(),
        "find": texts.iter().map(|t| matches(m.find_matches(t, &p.word_boundary_eligible))).collect::<Vec<_>>(),
        "findNoBounds": texts.iter().map(|t| matches(m.find_matches(t, &[]))).collect::<Vec<_>>(),
        "findWords": texts.iter().map(|t| matches(m.find_word_matches(t, &p.word_boundary_eligible))).collect::<Vec<_>>(),
    })
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().unwrap().iter().map(|s| s.as_str().unwrap().to_string()).collect()
}

/// Compares one golden item, naming the failing case by index (no text printed).
fn check(what: &str, index: usize, ours: &Value, golden: &Value, failures: &mut Vec<String>) {
    if ours != golden {
        failures.push(format!("{what}[{index}]"));
    }
}

#[test]
fn query_helpers_equal_the_old_engine() {
    let input = load("inputs.json");
    let golden = load("golden.json");
    let texts = strings(&input["texts"]);
    let mut failures = Vec::new();
    for (i, q) in strings(&input["queries"]).iter().enumerate() {
        let g = &golden["queries"][i];
        check("sanitize", i, &json!(otzaria::sanitize_query(q)), &g["sanitize"], &mut failures);
        check("split", i, &json!(otzaria::split_query_words(q)), &g["split"], &mut failures);
        let spans: Vec<Value> = otzaria::query_word_spans(q).into_iter().map(|(w, s, e)| json!([w, s, e])).collect();
        check("spans", i, &json!(spans), &g["spans"], &mut failures);
        check("literal", i, &json!(otzaria::literal_highlight_pattern(q)), &g["literal"], &mut failures);
        check("smartExact", i, &pattern(otzaria::smart_highlight_pattern(q, true), &texts), &g["smartExact"], &mut failures);
        check("smartFuzzy", i, &pattern(otzaria::smart_highlight_pattern(q, false), &texts), &g["smartFuzzy"], &mut failures);
    }
    assert!(failures.is_empty(), "differ from the old engine: {failures:?}");
}

#[test]
fn highlight_patterns_equal_the_old_engine() {
    let input = load("inputs.json");
    let golden = load("golden.json");
    let texts = strings(&input["texts"]);
    let mut failures = Vec::new();
    for (i, c) in input["highlight"].as_array().unwrap().iter().enumerate() {
        let spacing: HashMap<String, String> =
            c["customSpacing"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.as_str().unwrap().into())).collect();
        let alternatives: HashMap<u32, Vec<String>> =
            c["alternativeWords"].as_object().unwrap().iter().map(|(k, v)| (k.parse().unwrap(), strings(v))).collect();
        let options: HashMap<String, HashMap<String, bool>> = c["searchOptions"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.as_object().unwrap().iter().map(|(o, b)| (o.clone(), b.as_bool().unwrap())).collect()))
            .collect();
        let p = otzaria::highlight_pattern(
            c["query"].as_str().unwrap(),
            c["distance"].as_u64().unwrap() as u32,
            &spacing,
            &alternatives,
            &options,
            c["anyOrder"].as_bool().unwrap(),
        );
        check("highlight", i, &pattern(p, &texts), &golden["highlight"][i], &mut failures);
    }
    assert!(failures.is_empty(), "differ from the old engine: {failures:?}");
}

#[test]
fn user_regex_plan_and_matcher_equal_the_old_engine() {
    let input = load("inputs.json");
    let golden = load("golden.json");
    let texts = strings(&input["texts"]);
    let mut failures = Vec::new();
    for (i, raw) in strings(&input["regex"]).iter().enumerate() {
        let g = &golden["regex"][i];
        let plan = user_regex::plan(raw);
        let kind = match plan.kind {
            UserRegexKind::Invalid => "invalid",
            UserRegexKind::SingleWord => "singleWord",
            UserRegexKind::WordSequence => "wordSequence",
            UserRegexKind::CrossSpace => "crossSpace",
        };
        check("regexKind", i, &json!(kind), &g["kind"], &mut failures);
        check("regexPattern", i, &json!(plan.pattern), &g["pattern"], &mut failures);
        check("regexWords", i, &json!(plan.words), &g["words"], &mut failures);
        let found = match otzaria::UserRegexMatcher::prepare(raw) {
            None => Value::Null,
            Some(m) => Value::Array(texts.iter().map(|t| matches(m.find_matches(t))).collect()),
        };
        check("regexFind", i, &found, &g["find"], &mut failures);
    }
    assert!(failures.is_empty(), "differ from the old engine: {failures:?}");
}
