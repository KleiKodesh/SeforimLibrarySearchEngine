use super::*;

fn refs(lines: &[(&'static str, u64)]) -> Vec<LineRef<'static>> {
    lines.iter().map(|&(text, section)| LineRef { text, section }).collect()
}

fn hex(k: ChunkKey) -> String {
    k.0.iter().map(|b| format!("{b:02x}")).collect()
}

/// The two families in use: qa491 (anchor) and qa491-windows (model.json values).
#[test]
fn identities_are_the_published_ones() {
    let windows = ChunkerConfig::production();
    assert_eq!(windows.identity(), 320_260_894_818_558_297);
    assert_eq!(windows.anchor().identity(), 2_685_558_872_390_372_738);
    assert_eq!(ChunkerConfig::by_identity(320_260_894_818_558_297), Some(windows.clone()));
    assert_eq!(ChunkerConfig::by_identity(2_685_558_872_390_372_738), Some(windows.anchor()));
    assert_eq!(ChunkerConfig::by_identity(1), None);
}

/// chunk_key.rs `a_key_is_the_sha256_prefix_python_computes`.
#[test]
fn key_is_the_sha256_prefix() {
    let key = ChunkKey::of("[PASSAGE] a line that stands alone");
    assert_eq!(hex(key), "0429557a367f9f5e6681647f940ff66e");
    assert_eq!(key.column_value(), 299_864_833_585_553_246);
    let narrow = key.narrowed();
    assert!(narrow.same_text(key) && key.same_text(narrow) && !key.is_narrow());
    let mut twin = key;
    twin.0[15] ^= 1;
    assert!(!twin.same_text(key) && twin.same_text(narrow));
}

/// chunker.rs `the_production_recipe_keys_a_fixed_book_as_python_does` (cap 30).
#[test]
fn fixed_book_keys() {
    let chunker = Chunker::new(ChunkerConfig { max_chunk_chars: 30, ..ChunkerConfig::production().anchor() }).unwrap();
    let lines = refs(&[
        ("a line that stands alone", 1),
        ("short", 1),
        ("   ", 1),
        ("abcdefghijklmnopqrstuvwxyz0123456789", 2),
    ]);
    let keys: Vec<_> = (0..4).map(|i| chunker.embedded_text(&lines, i).map(|t| ChunkKey::of(&t))).collect();
    assert_eq!(hex(keys[0].unwrap()), "0429557a367f9f5e6681647f940ff66e");
    assert_eq!(keys[1], Some(ChunkKey::of("[PASSAGE] a line that stands alone short")));
    assert_eq!(keys[2], None);
    assert_eq!(keys[3], Some(ChunkKey::of("[PASSAGE] abcdefghijklmnopqrstuvwxyz0123")));
}

/// chunker.rs `windows_snap_to_words_and_stop_at_six` (ASCII forms of its cases).
#[test]
fn windows_snap_and_cap() {
    let chunker = Chunker::production();
    let spans = |text: &str| -> Vec<(u32, u32)> {
        chunker.embedded_windows(&[LineRef { text, section: 0 }], 0).iter().map(|w| (w.chars.start, w.chars.end)).collect()
    };
    assert_eq!(spans(&"x".repeat(512)), vec![(0, 512)]);
    let words = "abc ".repeat(129);
    let text = format!("{}z", &words[..512]);
    assert_eq!(spans(&text), vec![(0, 512), (384, 513)]);
    let word: String = (0..2000u32).map(|at| char::from_u32(0x61 + at * 7 % 26).unwrap()).collect();
    assert_eq!(spans(&word), vec![(0, 512), (384, 896), (768, 1280), (1152, 1664), (1536, 2000)]);
    let text = (0..1500).map(|i| format!("w{i:03}")).collect::<Vec<_>>().join(" ");
    assert_eq!(spans(&text).len(), 6);
}

#[test]
fn short_lines_borrow_their_section_only() {
    let chunker = Chunker::production();
    let lines = refs(&[("head line", 1), ("aaaaaa", 2), ("bbbbbb", 2), ("cccccc", 2), ("dddddd", 2), ("other", 3)]);
    assert_eq!(chunker.embedded_text(&lines, 1).unwrap(), "[PASSAGE] aaaaaa bbbbbb cccccc");
    assert_eq!(chunker.embedded_text(&lines, 3).unwrap(), "[PASSAGE] aaaaaa bbbbbb cccccc dddddd");
    let key = ChunkKey::of(&chunker.embedded_text(&lines, 2).unwrap());
    assert!(holds(&chunker, &lines, 2, 0, key));
    assert!(holds(&chunker, &lines, 2, 0, key.narrowed()));
    // Line 3 embeds the same four lines: it holds the key too.
    assert!(holds(&chunker, &lines, 3, 0, key));
    assert!(!holds(&chunker, &lines, 1, 0, key));
}

#[test]
fn rows_with_newlines_and_sections() {
    let rows: Vec<&[u8]> = vec![b"<h1>Title</h1>", b"one\ntwo<br>three", b"&nbsp; four  &amp; <b>x</b>y"];
    let lines = book_lines(&rows, &[]);
    let got: Vec<_> = lines.iter().map(|l| (l.row, l.sub, l.text.as_str(), l.section)).collect();
    assert_eq!(
        got,
        vec![(0, 0, "Title", 1), (1, 0, "one", 1), (1, 1, "two three", 1), (2, 0, "four xy", 1)]
    );
}

#[test]
fn marks_and_presentation_forms_are_folded() {
    // bet + dagesh (presentation form), shin + qamats (attached mark dropped), maqaf kept.
    assert_eq!(normalize_text_for_indexing("\u{FB31}\u{05E9}\u{05B8}\u{05BE}x"), "\u{05D1}\u{05E9}\u{05BE}x");
    // A sup around one letter inside a word is transparent; around digits it breaks.
    assert_eq!(normalize_text_for_indexing("\u{05D0}<sup>\u{05D1}</sup>\u{05D2}"), "\u{05D0}\u{05D1}\u{05D2}");
    assert_eq!(normalize_text_for_indexing("\u{05D0}<sup>30</sup>"), "\u{05D0} 30");
}

#[test]
fn footnotes_are_put_back() {
    let rows: Vec<&[u8]> = vec![b"word<sup class=\"footnote-marker\">1</sup> more"];
    let notes = vec![BookNote { line_index: 0, char_pos: -1, has_marker: true, body: "note".into() }];
    let lines = book_lines(&rows, &notes);
    assert_eq!(lines[0].text, "word 1 note more");
}

#[test]
fn data_uris_are_stripped() {
    let payload = "A".repeat(64);
    assert_eq!(strip_data_uris_for_index(&format!("a data:{payload} b")), "a  b");
    assert_eq!(strip_data_uris_for_index("a data:AAAA b"), "a data:AAAA b");
}

/// Real data (read-only): recompute every record's key of ~50 books of seforim.db
/// against the .oxv. Rows are dumped by the host-side script (no database code here):
/// `SE_CHUNKER_BOOKS` = JSON from `dump_books.py`, `SE_CHUNKER_OXV` = the .oxv.
#[test]
#[ignore]
fn real_data_match_rate() {
    use crate::oxv::OxvFile;
    let books = std::env::var("SE_CHUNKER_BOOKS").expect("SE_CHUNKER_BOOKS");
    let oxv = OxvFile::open(std::path::Path::new(&std::env::var("SE_CHUNKER_OXV").expect("SE_CHUNKER_OXV"))).unwrap();
    let index: HashMap<String, usize> = oxv.books().iter().enumerate().map(|(i, b)| (b.name.clone(), i)).collect();
    let dump: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(books).unwrap()).unwrap();
    let chunker = Chunker::production();
    let (mut all, mut ok) = (0u64, 0u64);
    for book in dump.as_array().unwrap() {
        let id = book["book_id"].as_i64().unwrap();
        let rows: Vec<Vec<u8>> = book["rows"].as_array().unwrap().iter().map(|r| b64(r["b64"].as_str().unwrap())).collect();
        let notes: Vec<BookNote> = book["notes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| BookNote {
                line_index: n[0].as_i64().unwrap(),
                char_pos: n[2].as_i64().unwrap(),
                has_marker: n[3].as_i64().unwrap() == 1,
                body: String::from_utf8_lossy(&b64(n[4].as_str().unwrap())).into_owned(),
            })
            .collect();
        let Some(&b) = index.get(&format!("id:{id}")) else {
            println!("book {id}: not in the .oxv");
            continue;
        };
        let row_refs: Vec<&[u8]> = rows.iter().map(|r| r.as_slice()).collect();
        let lines = book_lines(&row_refs, &notes);
        let line_refs: Vec<LineRef> = lines.iter().map(BookLine::as_ref).collect();
        // Primary records and extra records (a key repeated in the book), format 2:
        // no aliases, no occurrence snapshots.
        let book_info = &oxv.books()[b];
        let mut records: Vec<(u32, u32)> = book_info.slots.clone().map(|slot| (slot, oxv.hint(slot))).collect();
        records.extend(book_info.extras.clone().map(|i| oxv.extra(i)).filter(|&(slot, _)| slot < oxv.slot_count()));
        let (mut n, mut m) = (0u64, 0u64);
        for &(slot, hint) in &records {
            let window = oxv.slot_window(slot);
            if window == ALIAS_WINDOW {
                continue;
            }
            let key = oxv.key(slot);
            n += 1;
            if holds(&chunker, &line_refs, hint as usize, window, ChunkKey(key)) {
                m += 1;
            }
        }
        all += n;
        ok += m;
        println!(
            "book {id}: rows {} lines {} notes {} records {n} match {m} ({:.2}%)",
            rows.len(),
            lines.len(),
            notes.len(),
            if n == 0 { 100.0 } else { 100.0 * m as f64 / n as f64 }
        );
    }
    println!("TOTAL records {all} match {ok} ({:.3}%)", 100.0 * ok as f64 / all.max(1) as f64);
}

pub(crate) fn b64(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let bytes: Vec<u8> = s.bytes().filter(|&c| c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let v: Vec<u32> = chunk.iter().map(|&c| val(c) as u32).collect();
        let n = v.iter().fold(0u32, |a, &x| (a << 6) | x) << (6 * (4 - v.len()));
        out.push((n >> 16) as u8);
        if v.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if v.len() > 3 {
            out.push(n as u8);
        }
    }
    out
}

/// semantic_keys.rs `keyed_chunkings` with pardes; the identity is the sidecar's
/// `pardesqa491-windows-onnx/model.json` `chunking_identity`.
#[test]
fn pardes_identity_is_the_published_one() {
    let pardes = ChunkerConfig::pardes();
    assert_eq!(pardes.identity(), 13_334_152_532_150_206_612);
    assert_eq!(
        ChunkerConfig::keyed().map(|c| c.identity()),
        [320_260_894_818_558_297, 2_685_558_872_390_372_738, 13_334_152_532_150_206_612]
    );
    assert_eq!(ChunkerConfig::by_identity(13_334_152_532_150_206_612), Some(pardes.clone()));
    assert_eq!(ChunkerConfig::by_identity(pardes.anchor().identity()), None);
    assert!(Chunker::new(pardes).is_ok());
    assert!(Chunker::new(ChunkerConfig { embedding_text_version: 4, ..ChunkerConfig::production() }).is_err());
    assert!(Chunker::new(ChunkerConfig { normalization_version: 3, ..ChunkerConfig::production() }).is_err());
}

/// pardes_clean.rs `matches_python_reference_cases` (the sidecar's own cases file).
#[test]
fn pardes_clean_matches_python_reference_cases() {
    let cases: Vec<(String, String)> =
        serde_json::from_str(include_str!("../tests/data/pardes_clean_cases.json")).unwrap();
    assert!(!cases.is_empty());
    for (input, expected) in cases {
        assert_eq!(pardes_clean(&input), expected, "{input:?}");
    }
}

/// tests/pardes_recipe.rs `pardes_roles_clean_then_prefix_without_meivin_markers`,
/// passage side (the query side lives in the hosts' embedders).
#[test]
fn pardes_passage_is_cleaned_without_prefix() {
    let raw = "  <b>\u{05e9}\u{05f4}\u{05e1}</b>&nbsp;\u{feff}\u{05e8}\u{05f3}  ";
    let cleaned = pardes_clean(raw);
    assert_eq!(cleaned, "\u{05e9}\"\u{05e1} \u{05e8}'");
    let chunker = Chunker::new(ChunkerConfig::pardes()).unwrap();
    assert_eq!(chunker.passage_text(&cleaned), cleaned);
    assert_eq!(pardes_clean("<br>&nbsp; "), "");
}

/// A pardes line: window 0 and the later windows are cleaned and carry no prefix;
/// a line that cleans to nothing embeds nothing.
#[test]
fn pardes_windows_clean_every_window() {
    let chunker = Chunker::new(ChunkerConfig::pardes()).unwrap();
    let lines = refs(&[("  <b>a line</b> that &amp; stands   alone  ", 1), ("<br><i>&nbsp;</i>&#32;<p>", 1)]);
    assert_eq!(chunker.embedded_text(&lines, 0).as_deref(), Some("a line that & stands alone"));
    assert_eq!(chunker.embedded_text(&lines, 1), None);
    let long: String = (0..200).map(|i| format!("<b>w{i}</b> ")).collect();
    let lines = refs(&[(Box::leak(long.into_boxed_str()), 1)]);
    let windows = chunker.embedded_windows(&lines, 0);
    assert!(windows.len() > 1);
    for w in &windows {
        assert!(!w.text.contains('<') && !w.text.starts_with(PASSAGE_PREFIX) && w.text == w.text.trim());
    }
    let production = Chunker::production().embedded_windows(&lines, 0);
    assert_eq!(windows.iter().map(|w| &w.chars).collect::<Vec<_>>(), production.iter().map(|w| &w.chars).collect::<Vec<_>>());
}

#[test]
fn column_values_per_row_and_row_lines() {
    let rows: Vec<&[u8]> = vec![b"<h1>Title</h1>", b"a line that is long enough to stand\nab", b"another long line standing alone here"];
    let lines = book_lines(&rows, &[]);
    let refs: Vec<LineRef<'_>> = lines.iter().map(BookLine::as_ref).collect();
    let chunker = Chunker::production();
    let want: Vec<u64> = (0..lines.len()).map(|i| chunker.embedded_text(&refs, i).map_or(0, |t| ChunkKey::of(&t).column_value())).collect();
    let got = book_column_values(&rows, &[]);
    assert_eq!(got.iter().map(Vec::len).collect::<Vec<_>>(), vec![1, 2, 1]);
    assert_eq!(got.concat(), want);
    assert_eq!(row_lines(rows[0], &[]), vec![(lines[0].text.clone(), true)]);
    assert_eq!(row_lines(rows[1], &[]), vec![(lines[1].text.clone(), false), (lines[2].text.clone(), false)]);
}

/// search_engine.rs `line_dedup_hash`: Hebrew letters, ASCII alphanumerics
/// lower-cased; spaces and punctuation ignored; under 12 Hebrew letters = 0.
#[test]
fn line_dedup_hash_is_otzarias() {
    let twelve: String = (0..12).map(|i| char::from_u32(0x05D0 + i).unwrap()).collect();
    let eleven: String = twelve.chars().take(11).collect();
    assert_eq!(line_dedup_hash(&eleven), 0);
    let h = line_dedup_hash(&twelve);
    assert_ne!(h, 0);
    let spaced: String = twelve.chars().flat_map(|c| [c, ' ', ',']).collect();
    assert_eq!(line_dedup_hash(&spaced), h);
    assert_eq!(line_dedup_hash(&format!("{twelve} AB1")), line_dedup_hash(&format!("{twelve} ab1")));
    assert_ne!(line_dedup_hash(&format!("{twelve} 100")), line_dedup_hash(&format!("{twelve} 200")));
    let cols = book_line_columns(&[twelve.as_bytes(), b"x
y"], &[]);
    assert_eq!(cols.iter().map(Vec::len).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(cols[0][0].1, h);
}
