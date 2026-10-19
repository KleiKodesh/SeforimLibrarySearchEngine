# SeforimDB Search Engine

Tantivy-based Hebrew search engine producing FtsLib-compatible results
(KitveiHakodesh) with Otzaria's search options.

## Layout

- `tantivy/` — **fork** of tantivy 0.27.0 (KleiKodesh version). Commit to
  `main`; upstream reachable via `origin/main`.
- `hebrew-fts/` — the Hebrew layer, separate crate (`tantivy = { path = "../tantivy" }`).

## Done

### tantivy fork patch
- **MatchedTermsSink** (`src/query/automaton_weight.rs`): opt-in. A search with
  a sink attached also returns the concrete words the FST expansion matched
  (fuzzy/regex/any automaton query). Zero cost when off; one memcpy per
  matched term when on. Per-doc scoring loop untouched.

### hebrew-fts crate
- **Tokenizer** (`scanner.rs`, `tokenizer.rs`) — port of FtsLib
  HtmlWordScanner: HTML-aware, inline tags transparent, block
  tags/entities/maqaf split, words 2..29 letters, intra-word quotes kept as
  canonical `"`. Two variants: `hebrew` (nikud/te'amim stripped) and
  `hebrew_marked` (marks kept, for vocalized fields). Tokens carry byte +
  UTF-16 + visible-char offsets (for the snippet locator and the JS frontend).
- **SetAutomaton** (`set_automaton.rs`) — byte-trie automaton: FST walk visits
  only candidate strings; optional quote transparency.
- **Grammar walk** (`grammar.rs`, `grammar_query.rs`) — FtsLib GrammarExpander
  port (%word / word% / %word%): 34 prefixes, 32 suffixes, sofit stem.
- **Otzaria walk** (`walk.rs`, `variants.rs`) — `HebrewWalkQuery` +
  `WalkOptions`, one composite automaton:
  - קידומות דקדוקיות (combos ≤3 letters) + ארמיות (ד/דא/א)
  - סיומות דקדוקיות (+ותיהם; rare יא/תא in full morphology)
  - סיומות ארמיות: ה↔א, ם↔ן on word and suffix alternatives
  - כתיב מלא/חסר (internal י/ו, ≤32 variants)
  - חלק ממילה (≤3 any-letters/side; ≤2 for 4+-letter words)
  - קידומות/סיומות מתקדם (any letters, 3/5 before, 5/7 after; both ≡ חלק ממילה)
  - התעלם מגרשיים (רמב"ם ↔ רמבם)
  - שגיאות כתיב/OCR (confusion pairs, tunable in `variants.rs`)
  - ניקוד/טעמים: `ignore_nikud` / `ignore_teamim`, independent — whether
    בראשית matches בְּרֵאשִׁית and/or cantillated forms (on a `hebrew_marked`
    field); both off = exact vocalization

All walks work with MatchedTermsSink → search results + matched words in one
pass, feeding the (upcoming) FtsLib-style snippet/highlight stage.

## Next
- Snippet window locator port (tightest window, context, marks).
- Searcher glue: line docs, catalog order, word-distance filter, batching.
- C ABI for C#/Dart.

## Dev
- Build/test: `cargo test` in `hebrew-fts/` (rust-lld + debug=0 for fast cycles).
- FtsLib reference sources: KitveiHakodesh git HEAD (files deleted on disk).
