//! The search engine proper, with no database code: the host owns the
//! corpus and feeds it.
//!
//! - [`LineIndex`] / [`LineWriter`]: create or open an index, add / delete
//!   lines, commit with a host payload (resume watermark, provenance)
//!   stored atomically with the documents. Positions are optional
//!   ([`IndexOptions`]).
//! - [`LineIndex::search`]: every matching line in catalog order, plus a
//!   [`SnippetSession`] that locates FtsLib snippets in the texts the host
//!   fetches, applying the word-distance / order filters.

pub mod fm_expand;
pub mod fm_index;
pub mod fm_sidecar;
pub mod index;
pub mod search;

pub use fm_index::FmIndex;
pub use index::{IndexOptions, LineDoc, LineIndex, LineWriter};
pub use search::{
    ExcludeSpec, GroupSpec, PlacedHit, SearchRequest, SearchResult, Snippet, SnippetSession,
    WalkOptionsSpec, SNIPPET_FLAG_LEADING_ELLIPSIS, SNIPPET_FLAG_TRAILING_ELLIPSIS,
};

/// Engine errors.
#[derive(Debug)]
pub enum Error {
    Tantivy(tantivy::TantivyError),
    Io(std::io::Error),
    /// A request the index cannot serve (e.g. spacing without positions, an
    /// FM-index over a dictionary with too many distinct characters).
    Unsupported(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Tantivy(e) => write!(f, "{e}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Unsupported(m) => write!(f, "{m}"),
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

    fn line(id: u64, position: u64, book: u64, idx: u64, text: &str) -> LineDoc {
        LineDoc {
            line_id: id,
            position,
            book_id: book,
            line_index: idx,
            toc_entry_id: 100 + idx,
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
                line(1, 2, 2, 0, "פירוש על התורה"),
                line(4, 0, 1, 1, "והתורה ניתנה למשה"),
                line(6, 1, 1, 3, "תורה צוה לנו משה"),
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
        assert_eq!(result.hits[0].toc_entry_id, 101);

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
                line(1, 0, 1, 0, "משה קק תורה קק קק קק סיני"), // משה–1–תורה–3–סיני
                line(2, 1, 1, 1, "משה קק קק קק תורה סיני"),    // משה–3–תורה–0–סיני
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
            &[line(1, 0, 1, 0, "משה תורה")],
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
                line(1, 0, 1, 0, "משה קבל תורה מסיני"),
                line(2, 1, 1, 1, "משה אמר דבר אחד ואחר כך תורה"),
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
            &[line(1, 0, 1, 0, "שלום עליכם"), line(2, 1, 1, 1, "שלומכם רב")],
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
        w.add(&line(2, 1, 1, 1, "ברכה רבה")).unwrap();
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
                line(1, 0, 1, 0, "משה קבל תורה בהר סיני"), // משה, תורה, סיני
                line(2, 1, 1, 1, "תורה צוה לנו"),          // תורה
                line(3, 2, 1, 2, "סיני ואז משה"),           // סיני, משה (reverse order)
                line(4, 3, 1, 3, "דבר אחר"),
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
        // toc 100+idx: lines 1,2 share section 100 (idx 0), others their own;
        // lines 6,7 have no section (toc 0)
        let mut docs = vec![
            line(1, 0, 1, 0, "משה קבל תורה מסיני"),
            line(2, 1, 1, 0, "תורה ברכה גדולה"),
            line(3, 2, 1, 1, "ברכה תורה"),
            line(4, 3, 1, 2, "תורה בלי כלום"),
            line(5, 4, 1, 3, "ברכה ואז הרבה מילים ואז תורה"),
            line(6, 5, 1, 4, "ברכה תורה בלי כותרת"),
            line(7, 6, 1, 5, "תורה סתם"),
        ];
        docs[1].toc_entry_id = 100;
        docs[5].toc_entry_id = 0;
        docs[6].toc_entry_id = 0;
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
        // same section: line 1 shares section 100 with line 2 (ברכה) → both go;
        // a line without a section (toc 0) is never dropped, even with ברכה
        let r = index
            .search(&req(ExcludeSpec {
                groups: vec![word("ברכה", false)],
                same_section: true,
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(ids(&r), vec![4, 6, 7]);
        // same section, two words spread over two lines of section 100
        // (מסיני in line 1, ברכה in line 2): the section goes; distance ignored
        let r = index
            .search(&req(ExcludeSpec {
                groups: vec![word("ברכה", false), word("מסיני", false)],
                max_word_distance: Some(0),
                same_section: true,
                ..Default::default()
            }))
            .unwrap();
        assert_eq!(ids(&r), vec![3, 4, 5, 6, 7]);
        // an excluded word with no indexed form excludes nothing
        let r = index
            .search(&req(ExcludeSpec { groups: vec![word("זזזזז", false)], ..Default::default() }))
            .unwrap();
        assert_eq!(r.hits.len(), 7);
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
            lines.push(line(n, n, 1, n, &text));
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
        w.add(&line(n, n, 1, n, "אבאב")).unwrap();
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
}
