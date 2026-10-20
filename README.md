# SeforimDB Search Engine

Tantivy-based Hebrew search engine producing FtsLib-compatible results
(KitveiHakodesh) with Otzaria's search options.

## Layout

- `tantivy/` — **fork** of tantivy 0.27.0 (KleiKodesh version). Commit to
  `main`; upstream reachable via `origin/main`.
- `hebrew-fts/` — the Hebrew layer: tokenizer, walks, wildcard, snippet locator.
- `seforim-engine/` — the engine, **no database code**: line index fed by the
  host (optional positions; host payload committed atomically), search in
  catalog order, snippet session (filters, neighbour widening, term table).
- `engine-ffi/` — C ABI → `seforim_search_native.dll/.so/.dylib`.
- `csharp/SeforimSearch.Engine/` — .NET package (netstandard2.0: .NET 10 and
  .NET Framework 4.8) over the native library.
- `csharp/SeforimSearch.Demo/` — everything that reads seforim.db (copied from
  KitveiHakodesh Core) plus a FtsLib-shaped facade (`TantivySeforimIndex` ≈
  `Fts5SeforimIndex`, `SeforimDbFtsSearcher.SearchInBatches`, `FtsHit`,
  `SnippetDescriptor`, `QueryParser`). Targets net10.0 and net48.

Build: `cargo build --release -p engine-ffi`, then `dotnet build csharp/SeforimSearch.Demo -c Release`.
Run: `SeforimSearch.Demo build [--max-lines N] [--no-positions]`,
`SeforimSearch.Demo search [--prefixes] [--distance N] <query>`.

## Done

### tantivy fork patch
- **MatchedTermsSink** (`src/query/automaton_weight.rs`): opt-in. A search with
  a sink attached also returns the concrete words the FST expansion matched
  (fuzzy/regex/any automaton query). Zero cost when off; one memcpy per
  matched term when on. Per-doc scoring loop untouched.
- **SpanQuery** (`src/query/span_query.rs`): word-distance search where every
  slot is any term source (exact term, fuzzy, regex, any automaton incl. our
  walks). Per-gap limits (`a –2– b –5– c`, counted as words between),
  ordered or unordered, one word never fills two slots. Needs a field with
  `WithFreqsAndPositions`.

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
pass, feeding the snippet stage. Any walk becomes a span slot via
`HebrewWalkQuery::span_slot()`.
- **Snippet locator** (`prepared.rs`, `snippet.rs`) — step-for-step port of
  FtsLib `PreparedQueryGroups` + `SnippetWindowLocator` + `SnippetDescriptor`:
  one tokenize pass per line, tightest window covering all AND groups
  (strict-< tie rule), ±8 tokens context, 4000-visible-char ceiling that never
  cuts into the match, ellipsis flags, marks as (start, len, termOrdinal)
  triples, WordDistance / ordered check. Offsets are UTF-16 units, as FtsLib's,
  so the page renderer is unchanged. Term→group map built once per query from
  the per-group sinks.

### Expansion caps
Every walk (grammar, Otzaria options, wildcard, regex) is capped at 2000 terms
INSIDE the walk: the term dictionary is walked shortest-first, level by level
across all segments, and stops at the cap (ties: most frequent). Fuzzy keeps a
distance-ranked cap.

## Next
- Dart binding over the same C ABI.
- Side-by-side check against FtsLib on a full build.

## Dev
- Build/test: `cargo test` in `hebrew-fts/` (rust-lld + debug=0 for fast cycles).
- FtsLib reference sources: KitveiHakodesh git HEAD (files deleted on disk).
