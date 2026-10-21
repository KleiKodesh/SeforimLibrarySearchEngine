//! The search engine proper, with no database code: the host owns the
//! corpus and feeds it.
//!
//! - [`LineIndex`] / [`LineWriter`]: create or open an index, add / delete
//!   lines, commit with a host payload (resume watermark, provenance)
//!   stored atomically with the documents. Positions are optional
//!   ([`IndexOptions`]).
//! - [`LineIndex::search_options`]: THE search API, one [`SearchOptions`]
//!   (query string + word options, per-word overrides). [`LineIndex::search`]
//!   takes the low-level [`SearchRequest`] it lowers to. Either returns
//!   every matching line in catalog order, plus a [`SnippetSession`] that locates FtsLib snippets in the texts the host
//!   fetches, applying the word-distance / order filters.

pub mod fm_expand;
pub mod fm_index;
pub mod fm_sidecar;
pub mod index;
pub mod options;
pub mod relevance;
pub mod search;

pub use fm_index::FmIndex;
/// Otzaria's `keep_second_of_paired_readings` (its query sanitizer): every
/// paired reading `(X) [Y]` becomes ` Y `. The query parser runs it; hosts
/// that number the query's words themselves run it first.
pub use hebrew_fts::word_rule::keep_second_of_paired_readings as reduce_paired_readings;
/// Otzaria's user-regex kind of a query (see `hebrew_fts::user_regex::classify`).
pub use hebrew_fts::user_regex::classify as user_regex_kind;
pub use index::{IndexOptions, LineDoc, LineIndex, LineWriter};
pub use options::{ExcludeOptions, PhraseOptions, SearchOptions, WordMatchSpec, WordOptions};
pub use relevance::ResultsOrder;
pub use search::{
    CancelToken, ExcludeSpec, GroupSpec, HitCounts, PlacedHit, ProximitySpec, SearchRequest, SearchResult, Snippet, SnippetSession,
    TypoModeSpec, WalkOptionsSpec, SNIPPET_FLAG_LEADING_ELLIPSIS, SNIPPET_FLAG_TRAILING_ELLIPSIS,
};

/// Engine errors.
#[derive(Debug)]
pub enum Error {
    Tantivy(tantivy::TantivyError),
    Io(std::io::Error),
    /// A request the index cannot serve (e.g. spacing without positions, an
    /// FM-index over a dictionary with too many distinct characters).
    Unsupported(String),
    /// The search was cancelled through its [`CancelToken`].
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Tantivy(e) => write!(f, "{e}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Unsupported(m) => write!(f, "{m}"),
            Error::Cancelled => write!(f, "search cancelled"),
        }
    }
}

impl std::error::Error for Error {}

impl From<tantivy::TantivyError> for Error {
    fn from(e: tantivy::TantivyError) -> Self {
        Error::Tantivy(e)
    }
}
impl From<tantivy::directory::error::OpenDirectoryError> for Error {
    fn from(e: tantivy::directory::error::OpenDirectoryError) -> Self {
        Error::Tantivy(e.into())
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// Engine result.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: u64, position: u64, book: u64, text: &str) -> LineDoc {
        LineDoc {
            line_id: id,
            position,
            book_id: book,
            text: text.into(),
        }
    }

    fn word(w: &str, prefixes: bool) -> GroupSpec {
        GroupSpec::Word {
            word: w.into(),
            options: WalkOptionsSpec {
                grammar_prefixes: prefixes,
                ..Default::default()
            },
        }
    }

    fn build(dir: &std::path::Path, options: IndexOptions, lines: &[LineDoc]) -> LineIndex {
        let index = LineIndex::create(dir, options).unwrap();
        let mut w = index.writer(50_000_000, 2).unwrap();
        for l in lines {
            w.add(l).unwrap();
        }
        w.commit(Some("watermark=3")).unwrap();
        index
    }

    #[test]
    fn catalog_order_matched_words_snippets_and_payload() {
        let dir = tempfile::tempdir().unwrap();
        // ids out of catalog order: positions decide the order
        let index = build(
            dir.path(),
            IndexOptions::default(),
            &[
                line(1, 2, 2, "פירוש על התורה"),
                line(4, 0, 1, "והתורה ניתנה למשה"),
                line(6, 1, 1, "תורה צוה לנו משה"),
            ],
        );
        assert_eq!(index.committed_payload().unwrap().as_deref(), Some("watermark=3"));

        let mut result = index
            .search(&SearchRequest {
                groups: vec![word("תורה", true)],
                ..Default::default()
            })
            .unwrap();
        let ids: Vec<u64> = result.hits.iter().map(|h| h.line_id).collect();
        assert_eq!(ids, vec![4, 6, 1]);
        assert_eq!(result.hits[0].book_id, 1);

        let text = "והתורה ניתנה למשה";
        let s = result.session.locate(text).unwrap();
        assert_eq!(s.marks.len(), 3);
        let u16s: Vec<u16> = text.encode_utf16().collect();
        let (st, len, term) = (s.marks[0] as usize, s.marks[1] as usize, s.marks[2] as usize);
        assert_eq!(String::from_utf16(&u16s[st..st + len]).unwrap(), "והתורה");
        assert_eq!(result.session.all_terms()[term], "והתורה");
        assert!(s.is_short(result.session.context_words()));

        // a host that widens a short snippet simply locates a longer text
        let w = result.session.locate("בראשית ברא והתורה ניתנה למשה תורה צוה").unwrap();
        assert!(w.window_word_count > s.window_word_count);
        assert!(!result.session.take_new_terms().is_empty());
    }

    #[test]
    fn per_pair_gaps_run_as_span_query() {
        let dir = tempfile::tempdir().unwrap();
        let index = build(
            dir.path(),
            IndexOptions::default(),
            &[
                line(1, 0, 1, "משה קק תורה קק קק קק סיני"), // משה–1–תורה–3–סיני
                line(2, 1, 1, "משה קק קק קק תורה סיני"),    // משה–3–תורה–0–סיני
            ],
        );
        let req = |gaps: Vec<u32>| SearchRequest {
            groups: vec![word("משה", false), word("תורה", true), word("סיני", false)],
            gaps: Some(gaps),
            ..Default::default()
        };
        // a –2– b –5– c: only line 1 (line 2 fails the first gap)
        let mut r = index.search(&req(vec![2, 5])).unwrap();
        assert_eq!(r.hits.iter().map(|h| h.line_id).collect::<Vec<_>>(), vec![1]);
        // sinks still feed snippets
        assert!(r.session.locate("משה קק תורה קק קק קק סיני").is_some());
        // a –3– b –0– c: only line 2
        let r = index.search(&req(vec![3, 0])).unwrap();
        assert_eq!(r.hits.iter().map(|h| h.line_id).collect::<Vec<_>>(), vec![2]);
        // wrong gap count is an error
        assert!(index.search(&req(vec![1])).is_err());

        // an index without positions refuses per-pair spacing clearly
        let dir2 = tempfile::tempdir().unwrap();
        let flat = build(
            dir2.path(),
            IndexOptions { positions: false, keep_marks: false },
            &[line(1, 0, 1, "משה תורה")],
        );
        let err = flat
            .search(&SearchRequest {
                groups: vec![word("משה", false), word("תורה", false)],
                gaps: Some(vec![0]),
                ..Default::default()
            })
            .err()
            .unwrap();
        assert!(err.to_string().contains("positions"), "{err}");
    }

    /// Otzaria's word rule end to end, on both index flavours: the line ids a
    /// query finds (hits the locator confirms).
    #[test]
    fn otzaria_word_rules_on_both_flavours() {
        let long = "\u{05D0}".repeat(64); // 128 bytes: not indexed, one position
        let texts: Vec<String> = vec![
            "\u{05E4}.\u{05D1}.\u{05D9} \u{05D0}\u{05DE}\u{05E8}".into(),                       // 1: period joins
            "\u{05D0}[\u{05D1}]\u{05D2} \u{05D4}\u{05D5}".into(),                                // 2: brackets join
            "\u{05D3}\u{05D4}\u{200F}\u{05D5} \u{05D6}".into(),                                  // 3: bidi mark joins
            "abc\u{05D3}\u{05D4} Caf\u{00C9}".into(),                                             // 4: no script split
            "\u{05EA}\u{05D5}\u{05E1}' \u{05DB}\u{05EA}\u{05D1}".into(),                         // 5: trailing geresh
            "\u{05E8}\u{05E9}\u{201D}\u{05D9} \u{05D0}\u{05DE}\u{05E8}".into(),                  // 6: typographic quote
            "\u{FB2B}\u{05DC}\u{05D5}\u{05DD} \u{05E2}\u{05DC}\u{05D9}\u{05DB}\u{05DD}".into(),  // 7: presentation form
            "\u{05D5}\u{05D9}\u{05D0}\u{05DE}\u{05E8} (\u{05D4}\u{05D5}\u{05E6}\u{05D0}) [\u{05D4}\u{05D9}\u{05E6}\u{05D0}] \u{05D0}\u{05D5}\u{05EA}\u{05DD}".into(), // 8: paired reading
            format!("\u{05DE}\u{05E9}\u{05D4} {long} \u{05EA}\u{05D5}\u{05E8}\u{05D4}"),        // 9: too long word
        ];
        let lines: Vec<LineDoc> = texts.iter().enumerate().map(|(i, t)| line(i as u64 + 1, i as u64, 1, t)).collect();
        for positions in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let index = build(dir.path(), IndexOptions { positions, keep_marks: false }, &lines);
            let found = |o: SearchOptions| -> Vec<u64> {
                let mut r = index.search_options(&o).unwrap();
                let hits: Vec<u64> = r.hits.iter().map(|h| h.line_id).collect();
                hits.into_iter().filter(|&id| r.session.locate(&texts[id as usize - 1]).is_some()).collect()
            };
            let q = |s: &str| found(SearchOptions { query: s.into(), ..Default::default() });
            let ordered = |s: &str, gaps: Vec<u32>| {
                found(SearchOptions { query: s.into(), proximity: Some(ProximitySpec::Ordered { gaps }), ..Default::default() })
            };
            assert_eq!(q("\u{05E4}\u{05D1}\u{05D9}"), [1], "positions={positions}");
            assert_eq!(q("\u{05E4}.\u{05D1}.\u{05D9}"), [1]);
            assert_eq!(q("\u{05D0}\u{05D1}\u{05D2}"), [2]);
            assert_eq!(q("\u{05D3}\u{05D4}\u{05D5}"), [3]);
            assert_eq!(q("abc\u{05D3}\u{05D4}"), [4]);
            assert_eq!(q("abc"), Vec::<u64>::new());
            assert_eq!(q("caf\u{00E9}"), [4]);
            assert_eq!(q("\u{05EA}\u{05D5}\u{05E1}'"), [5]);
            assert_eq!(q("\u{05EA}\u{05D5}\u{05E1}"), [5]); // quote-free form
            assert_eq!(q("\u{05E8}\u{05E9}\"\u{05D9}"), [6]);
            assert_eq!(q("\u{05E8}\u{05E9}\u{05D9}"), [6]);
            assert_eq!(q("\u{05E9}\u{05DC}\u{05D5}\u{05DD}"), [7]);
            // the second reading at the first reading's position
            assert_eq!(ordered("\u{05D4}\u{05D9}\u{05E6}\u{05D0} \u{05D0}\u{05D5}\u{05EA}\u{05DD}", vec![0]), [8]);
            assert_eq!(ordered("\u{05D5}\u{05D9}\u{05D0}\u{05DE}\u{05E8} \u{05D4}\u{05D9}\u{05E6}\u{05D0}", vec![0]), [8]);
            assert_eq!(ordered("\u{05D5}\u{05D9}\u{05D0}\u{05DE}\u{05E8} \u{05D0}\u{05D5}\u{05EA}\u{05DD}", vec![0]), Vec::<u64>::new());
            // a query paired reading keeps its second reading
            assert_eq!(ordered("\u{05D5}\u{05D9}\u{05D0}\u{05DE}\u{05E8} (\u{05D4}\u{05D5}\u{05E6}\u{05D0}) [\u{05D4}\u{05D9}\u{05E6}\u{05D0}] \u{05D0}\u{05D5}\u{05EA}\u{05DD}", vec![0, 0]), [8]);
            // a too long word keeps its one position
            let three = format!("\u{05DE}\u{05E9}\u{05D4} {long} \u{05EA}\u{05D5}\u{05E8}\u{05D4}");
            assert_eq!(ordered(&three, vec![0, 0]), [9]);
            assert_eq!(ordered("\u{05DE}\u{05E9}\u{05D4} \u{05EA}\u{05D5}\u{05E8}\u{05D4}", vec![0]), Vec::<u64>::new());
            let fts = |s: &str| found(SearchOptions { query: s.into(), max_word_distance: Some(0), ..Default::default() });
            assert_eq!(fts(&three), [9]);
            assert_eq!(fts("\u{05DE}\u{05E9}\u{05D4} \u{05EA}\u{05D5}\u{05E8}\u{05D4}"), Vec::<u64>::new());
        }
    }

    #[test]
    fn distance_filter_without_positions() {
        // No positions stored: distance is validated from the text.
        let dir = tempfile::tempdir().unwrap();
        let index = build(
            dir.path(),
            IndexOptions {
                positions: false,
                keep_marks: false,
            },
            &[
                line(1, 0, 1, "משה קבל תורה מסיני"),
                line(2, 1, 1, "משה אמר דבר אחד ואחר כך תורה"),
            ],
        );
        let reopened = LineIndex::open(dir.path()).unwrap();
        assert!(!reopened.options().positions);
        let mut result = reopened
            .search(&SearchRequest {
                groups: vec![word("משה", false), word("תורה", false)],
                max_word_distance: Some(1),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.hits.len(), 2); // both contain both words
        assert!(result.session.locate("משה קבל תורה מסיני").is_some());
        assert!(result.session.locate("משה אמר דבר אחד ואחר כך תורה").is_none());
        drop(index);
    }

    #[test]
    fn update_and_wildcard_slots() {
        let dir = tempfile::tempdir().unwrap();
        let index = build(
            dir.path(),
            IndexOptions::default(),
            &[line(1, 0, 1, "שלום עליכם"), line(2, 1, 1, "שלומכם רב")],
        );
        let wildcard = |p: &str| GroupSpec::Wildcard {
            pattern: p.into(),
            ignore_quotes: false,
        };
        let result = index
            .search(&SearchRequest {
                groups: vec![wildcard("שלו*")],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.hits.len(), 2);

        // update line 2: delete + add new version
        let mut w = index.writer(50_000_000, 1).unwrap();
        w.delete(2);
        w.add(&line(2, 1, 1, "ברכה רבה")).unwrap();
        w.commit(None).unwrap();
        let result = index
            .search(&SearchRequest {
                groups: vec![wildcard("שלו*")],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.hits.len(), 1);

        // OR group: either word satisfies the slot; marks name the one present
        let mut result = index
            .search(&SearchRequest {
                groups: vec![GroupSpec::Any {
                    alternatives: vec![word("ברכה", false), word("שלום", false)],
                }],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.hits.len(), 2);
        let s = result.session.locate("ברכה רבה").unwrap();
        assert_eq!(result.session.all_terms()[s.marks[2] as usize], "ברכה");

        // fuzzy slot: שלם~1 reaches שלום (one insertion)
        let result = index
            .search(&SearchRequest {
                groups: vec![GroupSpec::Fuzzy {
                    word: "שלם".into(),
                    distance: 1,
                    ignore_quotes: false,
                }],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.hits.len(), 1);

        // locate_many = locate one by one, term ids included
        let texts: Vec<String> = (0..100)
            .map(|i| if i % 2 == 0 { "ברכה רבה".to_string() } else { "רבה ברכה".to_string() })
            .collect();
        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        let mut a = index
            .search(&SearchRequest {
                groups: vec![word("ברכה", false)],
                ..Default::default()
            })
            .unwrap();
        let mut b = index
            .search(&SearchRequest {
                groups: vec![word("ברכה", false)],
                ..Default::default()
            })
            .unwrap();
        let many = a.session.locate_many(&refs);
        let one: Vec<_> = refs.iter().map(|t| b.session.locate(t)).collect();
        assert_eq!(many, one);
        assert_eq!(a.session.all_terms(), b.session.all_terms());
        // byte-balanced chunks over very uneven lengths, non-matching lines,
        // and a second batch reusing the pooled locators: still identical
        let texts: Vec<String> = (0..997)
            .map(|i| match i % 5 {
                0 => format!("{}ברכה", "קק ".repeat(i * 7)),
                1 => "אין כאן".to_string(),
                2 => String::new(),
                3 => format!("רבה ברכה {}", "<b>א</b>ב&nbsp;".repeat(i % 50)),
                _ => "ברכה".repeat(i % 3 + 1),
            })
            .collect();
        let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
        for _ in 0..2 {
            let many = a.session.locate_many(&refs);
            let one: Vec<_> = refs.iter().map(|t| b.session.locate(t)).collect();
            assert_eq!(many, one);
        }
        assert_eq!(a.session.all_terms(), b.session.all_terms());

        // a rejected wildcard skips its slot rather than failing the search
        let result = index
            .search(&SearchRequest {
                groups: vec![wildcard("ש*"), word("שלום", false)],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(result.hits.len(), 1);
    }

    #[test]
    fn min_groups_partial_matching() {
        let dir = tempfile::tempdir().unwrap();
        let index = build(
            dir.path(),
            IndexOptions::default(),
            &[
                line(1, 0, 1, "משה קבל תורה בהר סיני"), // משה, תורה, סיני
                line(2, 1, 1, "תורה צוה לנו"),          // תורה
                line(3, 2, 1, "סיני ואז משה"),           // סיני, משה (reverse order)
                line(4, 3, 1, "דבר אחר"),
            ],
        );
        let groups = || vec![word("משה", false), word("תורה", false), word("סיני", false)];
        // all (None): only line 1
        let r = index.search(&SearchRequest { groups: groups(), ..Default::default() }).unwrap();
        assert_eq!(r.hits.iter().map(|h| h.line_id).collect::<Vec<_>>(), vec![1]);
        // mostWords = 3/2 + 1 = 2: lines 1 and 3; order/distance ignored
        let mut r = index
            .search(&SearchRequest {
                groups: groups(),
                min_groups: Some(2),
                require_ordered: true,
                max_word_distance: Some(0),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.hits.iter().map(|h| h.line_id).collect::<Vec<_>>(), vec![1, 3]);
        let s = r.session.locate("סיני ואז משה").unwrap();
        assert_eq!(s.marks.len(), 6);
        assert_eq!(s.word_distance, 1);
        // tightest 2-group window in line 1: "משה קבל תורה" (strict-< tie: earliest)
        let s1 = r.session.locate("משה קבל תורה בהר סיני").unwrap();
        assert_eq!(s1.word_distance, 1);
        // marks cover every query term inside the context-expanded window
        assert_eq!(s1.marks.len(), 9);
        // anyWord = 1: lines 1, 2, 3
        let r = index
            .search(&SearchRequest { groups: groups(), min_groups: Some(1), ..Default::default() })
            .unwrap();
        assert_eq!(r.hits.len(), 3);
        // a group with no indexed term does not sink the others
        let mut g = groups();
        g.push(word("זזזזז", false));
        let r = index
            .search(&SearchRequest { groups: g, min_groups: Some(3), ..Default::default() })
            .unwrap();
        assert_eq!(r.hits.iter().map(|h| h.line_id).collect::<Vec<_>>(), vec![1]);
        // min >= n means all
        let r = index
            .search(&SearchRequest { groups: groups(), min_groups: Some(7), ..Default::default() })
            .unwrap();
        assert_eq!(r.hits.len(), 1);
    }

    #[test]
    fn exclude_negative_query() {
        let dir = tempfile::tempdir().unwrap();
        let docs = vec![
            line(1, 0, 1, "משה קבל תורה מסיני"),
            line(2, 1, 1, "תורה ברכה גדולה"),
            line(3, 2, 1, "ברכה תורה"),
            line(4, 3, 1, "תורה בלי כלום"),
            line(5, 4, 1, "ברכה ואז הרבה מילים ואז תורה"),
            line(6, 5, 1, "ברכה תורה בלי כותרת"),
            line(7, 6, 1, "תורה סתם"),
        ];
        let index = build(dir.path(), IndexOptions::default(), &docs);
        let ids = |r: &SearchResult| r.hits.iter().map(|h| h.line_id).collect::<Vec<_>>();
        let req = |ex: ExcludeSpec| SearchRequest {
            groups: vec![word("תורה", false)],
            exclude: Some(ex),
            ..Default::default()
        };
        // one excluded word: query-level
        let r = index
            .search(&req(ExcludeSpec { groups: vec![word("ברכה", false)], ..Default::default() }))
            .unwrap();
        assert_eq!(ids(&r), vec![1, 4, 7]);
        // two words, no limit: query-level AND
        let r = index
            .search(&req(ExcludeSpec {
                groups: vec![word("ברכה", false), word("ואז", false)],
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(ids(&r), vec![1, 2, 3, 4, 6, 7]);
        // two words bound by distance 0: checked on the text at locate time
        let mut r = index
            .search(&req(ExcludeSpec {
                groups: vec![word("ברכה", false), word("תורה", false)],
                max_word_distance: Some(0),
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(ids(&r), vec![1, 2, 3, 4, 5, 6, 7]);
        assert!(r.session.locate("ברכה תורה").is_none()); // adjacent: excluded
        assert!(r.session.locate("תורה ברכה גדולה").is_none()); // adjacent, any order
        assert!(r.session.locate("ברכה ואז הרבה מילים ואז תורה").is_some()); // too far apart
        let many = r.session.locate_many(&["ברכה תורה", "משה קבל תורה מסיני"]);
        assert!(many[0].is_none() && many[1].is_some());
        // ordered: ברכה then תורה only
        let mut r = index
            .search(&req(ExcludeSpec {
                groups: vec![word("ברכה", false), word("תורה", false)],
                max_word_distance: Some(0),
                require_ordered: true,
                ..Default::default()
            }))
            .unwrap();
        assert!(r.session.locate("ברכה תורה").is_none());
        assert!(r.session.locate("תורה ברכה גדולה").is_some());
        // per-pair spacing: span query, query-level
        let r = index
            .search(&req(ExcludeSpec {
                groups: vec![word("ברכה", false), word("תורה", false)],
                gaps: Some(vec![0]),
                require_ordered: true,
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(ids(&r), vec![1, 2, 4, 5, 7]);
        // an excluded word with no indexed form excludes nothing
        let r = index
            .search(&req(ExcludeSpec { groups: vec![word("זזזזז", false)], ..Default::default() }))
            .unwrap();
        assert_eq!(r.hits.len(), 7);
    }

    #[test]
    fn phrase_alternatives_or_with_main_query() {
        for positions in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let index = build(
                dir.path(),
                IndexOptions { positions, keep_marks: false },
                &[
                    line(1, 0, 1, "כתב רמב\"ם בהלכות"),
                    line(2, 1, 1, "אמר רבי משה בן מיימון כך"),
                    line(3, 2, 1, "רבי יוסי בן משה מיימון"), // words, not the phrase
                    line(4, 3, 1, "דבר אחר"),
                ],
            );
            let phrase = |ws: &[&str]| ws.iter().map(|w| word(w, false)).collect::<Vec<_>>();
            let mut r = index
                .search(&SearchRequest {
                    groups: vec![GroupSpec::Word {
                        word: "רמב\"ם".into(),
                        options: WalkOptionsSpec::default(),
                    }],
                    phrase_alternatives: vec![phrase(&["רבי", "משה", "בן", "מיימון"])],
                    ..Default::default()
                })
                .unwrap();
            let ids: Vec<u64> = r.hits.iter().map(|h| h.line_id).collect();
            if positions {
                assert_eq!(ids, vec![1, 2], "span phrase");
            } else {
                // without positions line 3 is a candidate; the text check drops it
                assert_eq!(ids, vec![1, 2, 3]);
            }
            assert!(r.session.locate("כתב רמב\"ם בהלכות").is_some());
            let s = r.session.locate("אמר רבי משה בן מיימון כך").unwrap();
            assert_eq!(s.marks.len(), 12);
            let terms: Vec<String> =
                s.marks.chunks(3).map(|m| r.session.all_terms()[m[2] as usize].clone()).collect();
            assert_eq!(terms, vec!["רבי", "משה", "בן", "מיימון"]);
            assert!(r.session.locate("רבי יוסי בן משה מיימון").is_none());
            let many = r.session.locate_many(&["כתב רמב\"ם בהלכות", "רבי יוסי בן משה מיימון"]);
            assert!(many[0].is_some() && many[1].is_none());
        }
        // main query that cannot match: the phrase alone still answers
        let dir = tempfile::tempdir().unwrap();
        let index = build(dir.path(), IndexOptions::default(), &[line(1, 0, 1, "רבי משה בן מיימון")]);
        let mut r = index
            .search(&SearchRequest {
                groups: vec![word("זזזזז", false)],
                phrase_alternatives: vec![vec![word("משה", false), word("בן", false)]],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.hits.len(), 1);
        assert!(r.session.locate("רבי משה בן מיימון").is_some());
    }

    #[test]
    fn books_filter() {
        for positions in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let index = build(
                dir.path(),
                IndexOptions { positions, keep_marks: false },
                &[
                    line(1, 0, 1, "תורה ברכה"),
                    line(2, 1, 2, "תורה סתם"),
                    line(3, 2, 3, "רבי משה בן מיימון"),
                    line(4, 3, 2, "רבי משה בן מיימון"),
                    line(5, 4, 3, "תורה ברכה"),
                ],
            );
            let ids = |r: &SearchResult| r.hits.iter().map(|h| h.line_id).collect::<Vec<_>>();
            let req = |books: Option<Vec<u64>>| SearchRequest {
                groups: vec![word("תורה", false)],
                books,
                ..Default::default()
            };
            assert_eq!(ids(&index.search(&req(None)).unwrap()), vec![1, 2, 5]);
            assert_eq!(ids(&index.search(&req(Some(vec![2, 3]))).unwrap()), vec![2, 5]);
            assert!(index.search(&req(Some(vec![]))).unwrap().hits.is_empty());
            // with exclusion
            let r = index
                .search(&SearchRequest {
                    exclude: Some(ExcludeSpec { groups: vec![word("ברכה", false)], ..Default::default() }),
                    ..req(Some(vec![1, 2]))
                })
                .unwrap();
            assert_eq!(ids(&r), vec![2]);
            // with phrase alternatives
            let r = index
                .search(&SearchRequest {
                    phrase_alternatives: vec![vec![word("משה", false), word("בן", false)]],
                    ..req(Some(vec![2]))
                })
                .unwrap();
            assert_eq!(ids(&r), vec![2, 4]);
            assert!(r.hits.iter().all(|h| h.book_id == 2));
        }
    }

    /// An updated line (delete + add) must come back only in its new
    /// version, also when its segment keeps other live docs. Search used to
    /// collect through `for_each_no_score`, which does not skip deleted
    /// docs: with both lines in one segment (one indexing thread) the old
    /// version was found. It looked like a 1-in-9 flake because with two
    /// threads the old line usually sat alone in a segment, which tantivy
    /// drops whole once fully deleted.
    #[test]
    fn deleted_docs_are_not_hits() {
        let dir = tempfile::tempdir().unwrap();
        let index = LineIndex::create(dir.path(), IndexOptions::default()).unwrap();
        let mut w = index.writer(50_000_000, 1).unwrap();
        w.add(&line(1, 0, 1, "שלום עליכם")).unwrap();
        w.add(&line(2, 1, 1, "שלומכם רב")).unwrap();
        w.commit(None).unwrap();
        w.delete(2);
        w.add(&line(2, 1, 1, "ברכה רבה")).unwrap();
        w.commit(None).unwrap();
        let req = |p: &str| SearchRequest {
            groups: vec![GroupSpec::Wildcard { pattern: p.into(), ignore_quotes: false }],
            ..Default::default()
        };
        let ids = |r: SearchResult| r.hits.iter().map(|h| h.line_id).collect::<Vec<_>>();
        assert_eq!(ids(index.search(&req("שלו*")).unwrap()), vec![1]);
        assert_eq!(ids(index.search(&req("ברכ*")).unwrap()), vec![2]);
        // a plain delete, no re-add
        w.delete(1);
        w.commit(None).unwrap();
        assert!(index.search(&req("שלו*")).unwrap().hits.is_empty());
    }

    /// The parity diff that found the bug: FtsLib's quote-free index has
    /// יציק, one edit from יצחק; ours keeps יצי"ק, which the DFA counted as two
    /// edits. With the quote transparent it is one edit on both paths.
    #[test]
    fn fuzzy_quote_is_not_an_edit_when_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let index = build(
            dir.path(),
            IndexOptions::default(),
            &[line(1, 0, 1, "ויציק יצי\"ק שמן"), line(2, 1, 1, "אבגד")],
        );
        let fuzzy = |ignore_quotes: bool| SearchRequest {
            groups: vec![GroupSpec::Fuzzy { word: "יצחק".into(), distance: 1, ignore_quotes }],
            ..Default::default()
        };
        for fm in [false, true] {
            if fm {
                let mut w = index.writer(50_000_000, 1).unwrap();
                w.commit(None).unwrap();
                w.merge_all().unwrap();
                assert!(index.build_fm_sidecar().unwrap());
            }
            assert!(index.search(&fuzzy(false)).unwrap().hits.is_empty(), "fm={fm}: quote counts as an edit");
            let mut r = index.search(&fuzzy(true)).unwrap();
            assert_eq!(r.hits.len(), 1, "fm={fm}");
            let s = r.session.locate("ויציק יצי\"ק שמן").unwrap();
            let terms: Vec<String> = s.marks.chunks(3).map(|m| r.session.all_terms()[m[2] as usize].clone()).collect();
            assert_eq!(terms, vec!["יצי\"ק"], "fm={fm}");
        }
    }

    /// The FM-index sidecar must give EXACTLY the walk's terms (and hits) for
    /// every shape it serves, cap included. `test_only_disable_fm` exists for
    /// this comparison alone.
    #[test]
    fn fm_sidecar_equals_the_walk() {
        use std::sync::atomic::Ordering;
        let letters: Vec<char> = "אבגדהוזחטיכלמנסעפצקרשת".chars().collect();
        // > 2000 terms containing אב (cap exercised), some repeated (doc-count
        // ties), quoted forms, short and long words.
        let mut lines: Vec<LineDoc> = Vec::new();
        let mut n = 0u64;
        let mut push = |text: String, lines: &mut Vec<LineDoc>| {
            lines.push(line(n, n, 1, &text));
            n += 1;
        };
        for i in 0..letters.len() {
            for j in 0..letters.len() {
                for k in (0..letters.len()).step_by(3) {
                    let w = format!("{}{}אב{}", letters[i], letters[j], letters[k]);
                    let repeat = 1 + (i + j) % 3;
                    for _ in 0..repeat {
                        push(w.clone(), &mut lines);
                    }
                }
            }
        }
        for w in [
            "שלום", "שלומכם", "לום", "של\"ום", "רשי", "רש\"י", "רשיי", "רמב\"ם", "רמב\"ן", "רמבם",
            "ישראלאב", "אבג", "אב", "יצחק", "ויצחק", "ליצחק", "יצח\"ק", "יצחקים", "אבאבאב",
        ] {
            push(w.into(), &mut lines);
        }

        let dir = tempfile::tempdir().unwrap();
        let index = LineIndex::create(dir.path(), IndexOptions::default()).unwrap();
        let mut w = index.writer(60_000_000, 4).unwrap();
        for l in &lines {
            w.add(l).unwrap();
        }
        w.commit(None).unwrap();
        w.merge_all().unwrap();
        assert!(!index.has_fm_sidecar());
        assert!(index.build_fm_sidecar().unwrap());
        assert!(index.has_fm_sidecar());

        let wildcard = |p: &str, ignore_quotes: bool| GroupSpec::Wildcard {
            pattern: p.into(),
            ignore_quotes,
        };
        let fuzzy = |w: &str, d: u8| GroupSpec::Fuzzy {
            word: w.into(),
            distance: d,
            ignore_quotes: false,
        };
        let fuzzy_q = |w: &str, d: u8| GroupSpec::Fuzzy {
            word: w.into(),
            distance: d,
            ignore_quotes: true,
        };
        let groups: Vec<GroupSpec> = vec![
            wildcard("*אב", false),
            wildcard("*אב*", false),
            wildcard("*אב*", true),
            wildcard("*לום", false),
            wildcard("*לום", true),
            wildcard("*יצח*", false),
            wildcard("*יצח*", true),
            wildcard("*רש\"י", false),
            wildcard("*רש\"י", true),
            wildcard("*שי", false),
            wildcard("*שי", true),
            wildcard("*זזז*", false),
            fuzzy("שלום", 1),
            fuzzy("שלום", 2),
            fuzzy("רשי", 1),
            fuzzy("רמב\"ם", 1),
            fuzzy("רמבם", 1),
            fuzzy("יצחק", 2),
            fuzzy("אב", 2),
            fuzzy("זזזז", 1),
            // quote transparent: FM path (plain Levenshtein on quote-free
            // forms) must equal the walk (DFA skipping the quote byte)
            fuzzy_q("רשי", 1),
            fuzzy_q("רמב\"ם", 1),
            fuzzy_q("שלום", 2),
            fuzzy_q("יצחק", 2),
            // FtsLib's distance 3
            fuzzy("שלום", 3),
            fuzzy_q("יצחק", 3),
        ];
        for g in &groups {
            let run = |disable_fm: bool| {
                index.test_only_disable_fm.store(disable_fm, Ordering::Relaxed);
                let mut r = index
                    .search(&SearchRequest {
                        groups: vec![g.clone()],
                        ..Default::default()
                    })
                    .unwrap();
                let ids: Vec<u64> = r.hits.iter().map(|h| h.line_id).collect();
                let mut terms: Vec<String> = Vec::new();
                // every matched term, via the snippets of every hit
                for h in &r.hits {
                    if let Some(s) = r.session.locate(&lines[h.line_id as usize].text) {
                        for m in s.marks.chunks_exact(3) {
                            terms.push(r.session.all_terms()[m[2] as usize].clone());
                        }
                    }
                }
                terms.sort();
                terms.dedup();
                (ids, terms)
            };
            let fm = run(false);
            let walk = run(true);
            assert_eq!(fm, walk, "group {g:?}");
        }
        index.test_only_disable_fm.store(false, Ordering::Relaxed);
        // the cap really bit: more than 2000 terms contain אב
        let mut r = index
            .search(&SearchRequest {
                groups: vec![wildcard("*אב*", false)],
                ..Default::default()
            })
            .unwrap();
        let mut seen = std::collections::HashSet::new();
        for h in &r.hits {
            if let Some(s) = r.session.locate(&lines[h.line_id as usize].text) {
                seen.insert(s.marks[2]);
            }
        }
        assert_eq!(seen.len(), 2000);

        // a second commit makes two segments: the sidecar steps aside, the
        // walk still answers; rebuilding after a merge brings it back
        let mut w = index.writer(60_000_000, 1).unwrap();
        w.add(&line(n, n, 1, "אבאב")).unwrap();
        w.commit(None).unwrap();
        assert!(!index.has_fm_sidecar());
        let r = index
            .search(&SearchRequest {
                groups: vec![wildcard("*לום", false)],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(r.hits.len(), 2);
        w.merge_all().unwrap();
        assert!(index.build_fm_sidecar().unwrap());
        assert!(index.has_fm_sidecar());
    }

    #[test]
    fn exact_counts_total_and_by_book() {
        // moshe, torah, and filler words (escapes keep the source ASCII)
        const A: &str = "\u{05de}\u{05e9}\u{05d4}";
        const B: &str = "\u{05ea}\u{05d5}\u{05e8}\u{05d4}";
        const X: &str = "\u{05d0}\u{05de}\u{05e8}";
        let texts = [
            format!("{A} {B}"),
            format!("{A} {X} {X} {X} {B}"),
            format!("{B} {A}"),
            format!("{A} {X} {B}"),
            format!("{B} {X}"),
        ];
        for positions in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let lines: Vec<LineDoc> = texts
                .iter()
                .enumerate()
                .map(|(i, t)| line(i as u64, i as u64, 10 + (i as u64 % 2), t))
                .collect();
            let index = build(dir.path(), IndexOptions { positions, keep_marks: false }, &lines);
            let two = || vec![word(A, false), word(B, false)];
            let cases: Vec<(SearchRequest, u64)> = vec![
                (SearchRequest { groups: two(), ..Default::default() }, 4),
                (SearchRequest { groups: two(), max_word_distance: Some(1), ..Default::default() }, 3),
                (SearchRequest { groups: two(), require_ordered: true, ..Default::default() }, 3),
                (
                    SearchRequest {
                        groups: two(),
                        proximity: Some(ProximitySpec::Ordered { gaps: vec![0] }),
                        ..Default::default()
                    },
                    1,
                ),
                (
                    SearchRequest {
                        groups: two(),
                        exclude: Some(ExcludeSpec {
                            groups: vec![word(B, false), word(A, false)],
                            require_ordered: true,
                            max_word_distance: Some(0),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    3,
                ),
                (
                    SearchRequest {
                        groups: two(),
                        proximity: Some(ProximitySpec::Ordered { gaps: vec![1] }),
                        ..Default::default()
                    },
                    2,
                ),
                (
                    SearchRequest {
                        groups: two(),
                        proximity: Some(ProximitySpec::AnyOrder { distance: 0 }),
                        ..Default::default()
                    },
                    2,
                ),
                (SearchRequest { groups: vec![word(B, false)], ..Default::default() }, 5),
                // one group: a distance has nothing to check, no text needed
                (SearchRequest { groups: vec![word(B, false)], max_word_distance: Some(0), ..Default::default() }, 5),
                (SearchRequest { groups: vec![word(B, false)], require_ordered: true, ..Default::default() }, 5),
            ];
            for (req, want) in cases {
                let mut r = index.search(&req).unwrap();
                // the reference: every hit located one by one
                let mut by_book = std::collections::BTreeMap::<u64, u64>::new();
                for h in &r.hits {
                    if r.session.locate(&texts[h.line_id as usize]).is_some() {
                        *by_book.entry(h.book_id).or_default() += 1;
                    }
                }
                let reference: u64 = by_book.values().sum();
                assert_eq!(reference, want, "positions={positions} {req:?}");
                // no text-level check left: plain AND, one group, spans on positions
                let no_text = req.max_word_distance.is_none() && !req.require_ordered && req.exclude.is_none()
                    && (req.proximity.is_none() || positions)
                    || req.groups.len() == 1;
                assert_eq!(r.count_needs_text(), !no_text, "positions={positions} {req:?}");
                let hit_texts: Vec<&str> = r.hits.iter().map(|h| texts[h.line_id as usize].as_str()).collect();
                if r.count_needs_text() {
                    assert!(!r.counts().exact);
                    // two batches, continuing in catalog order
                    let (a, b) = hit_texts.split_at(hit_texts.len() / 2);
                    r.count_batch(a).unwrap();
                    r.count_batch(b).unwrap();
                    assert!(r.count_batch(&[""]).is_err(), "past the last hit");
                } else {
                    // no text-level check: the hit list itself is exact
                    assert_eq!(r.hits.len() as u64, want, "positions={positions} {req:?}");
                }
                let c = r.counts();
                assert!(c.exact);
                assert_eq!(c.total, want, "positions={positions} {req:?}");
                assert_eq!(c.books, by_book.into_iter().collect::<Vec<_>>());
            }
        }
    }

    #[test]
    fn cancel_token_stops_search_locate_and_count() {
        const B: &str = "\u{05ea}\u{05d5}\u{05e8}\u{05d4}";
        let dir = tempfile::tempdir().unwrap();
        let lines: Vec<LineDoc> = (0..5000).map(|i| line(i, i, 1, &format!("{B} {i}"))).collect();
        let index = build(dir.path(), IndexOptions::default(), &lines);
        let req = SearchRequest {
            groups: vec![word(B, false), word("1", false)],
            max_word_distance: Some(0),
            ..Default::default()
        };
        let plain = SearchRequest { groups: vec![word(B, false)], ..Default::default() };
        // not cancelled: same hits as the plain search, through the scorer loop
        let token = CancelToken::new();
        let r = index.search_cancellable(&plain, Some(token.clone())).unwrap();
        assert_eq!(r.hits, index.search(&plain).unwrap().hits);
        assert_eq!(r.hits.len(), 5000);
        let mut r = index.search_cancellable(&req, Some(token.clone())).unwrap();
        assert_eq!(r.hits.len(), 1);
        let texts: Vec<String> = lines.iter().map(|l| l.text.clone()).collect();
        let refs: Vec<&str> = texts.iter().map(|t| t.as_str()).collect();
        assert!(r.session.try_locate_many(&refs[1..2]).unwrap()[0].is_some());
        token.cancel();
        assert!(matches!(r.session.try_locate_many(&refs), Err(Error::Cancelled)));
        assert!(matches!(r.count_batch(&refs[..1]), Err(Error::Cancelled)));
        assert!(matches!(index.search_cancellable(&plain, Some(token)), Err(Error::Cancelled)));
    }

    #[test]
    fn user_regex_refused_on_a_marks_index() {
        let dir = tempfile::tempdir().unwrap();
        let index = build(dir.path(), IndexOptions { positions: true, keep_marks: true }, &[line(1, 0, 1, "abc")]);
        let o = SearchOptions { query: "/ab./".into(), user_regex: true, ..Default::default() };
        assert!(matches!(index.search_options(&o), Err(Error::Unsupported(_))));
    }

    #[test]
    fn user_regex_queries_walk_the_dictionary() {
        // shalom, shalem, sh-l-m with quote, olam, and filler
        const SHALOM: &str = "\u{05e9}\u{05dc}\u{05d5}\u{05dd}";
        const SHALEM: &str = "\u{05e9}\u{05dc}\u{05dd}";
        const QUOTED: &str = "\u{05e9}\u{05dc}\"\u{05dd}";
        const OLAM: &str = "\u{05e2}\u{05d5}\u{05dc}\u{05dd}";
        let texts = [
            format!("{SHALOM} {OLAM}"),
            format!("{SHALEM} x {OLAM}"),
            format!("{QUOTED}"),
            format!("{OLAM} {SHALOM}"),
            "Abc abd".to_string(),
        ];
        for positions in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let lines: Vec<LineDoc> = texts.iter().enumerate().map(|(i, t)| line(i as u64, i as u64, 1, t)).collect();
            let index = build(dir.path(), IndexOptions { positions, keep_marks: false }, &lines);
            let found = |q: &str| -> Vec<u64> {
                let mut r = index
                    .search_options(&SearchOptions { query: q.into(), user_regex: true, ..Default::default() })
                    .unwrap();
                let hits = r.hits.clone();
                hits.iter()
                    .filter(|h| r.session.locate(&texts[h.line_id as usize]).is_some())
                    .map(|h| h.line_id)
                    .collect()
            };
            // one word: whole-term, quote transparent
            assert_eq!(found("/\u{05e9}\u{05dc}\u{05d5}?\u{05dd}/"), vec![0, 1, 2, 3], "positions={positions}");
            assert_eq!(found("/\u{05e9}\u{05dc}/"), Vec::<u64>::new());
            // case-insensitive, classes
            assert_eq!(found("/AB[CD]/"), vec![4]);
            // a sequence: adjacent, in order
            assert_eq!(found("/\u{05e9}\u{05dc}\u{05d5}?\u{05dd} \u{05e2}\u{05d5}\u{05dc}\u{05dd}/"), vec![0], "positions={positions}");
            // an invalid pattern is plain text (no hit here), the line kind an error
            assert_eq!(found("/a(b/"), Vec::<u64>::new());
            assert!(index
                .search_options(&SearchOptions { query: "/a[ ,]b/".into(), user_regex: true, ..Default::default() })
                .is_err());
            // off (FtsLib's default): the slashes are plain text, no regex
            let plain = SearchOptions { query: "/AB[CD]/".into(), ..Default::default() };
            assert!(index.search_options(&plain).unwrap().hits.is_empty());
            // a bounded Unicode-class repeat walks narrowed, the verifier keeps the bound
            assert_eq!(found("/\\w{3}/"), vec![1, 2, 4]);
            // the regex sequence is exact on both flavours, counts included
            let mut r = index
                .search_options(&SearchOptions {
                    query: "/\u{05e9}\u{05dc}\u{05d5}?\u{05dd} \u{05e2}\u{05d5}\u{05dc}\u{05dd}/".into(),
                    user_regex: true,
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(r.count_needs_text(), !positions);
            let hit_texts: Vec<&str> = r.hits.iter().map(|h| texts[h.line_id as usize].as_str()).collect();
            r.count_batch(&hit_texts).unwrap();
            assert_eq!(r.counts().total, 1);
        }
    }
}
