# Dart side

Two packages:

| Folder | What |
|---|---|
| `seforim_search/` | `dart:ffi` binding to `seforim_search_native` (the C ABI in `engine-ffi/src/lib.rs`). Mirrors the C# `SeforimSearch.Engine` package. |
| `otzaria_demo/` | A console app that does what Otzaria's full-text search does, on this engine. It reads seforim.db with `package:sqlite3` and decodes zstd through `libzstd`. |

## Build and run

```
cargo build --release -p engine-ffi            # repo root → target/release/seforim_search_native.dll

cd dart/seforim_search
dart pub get
dart test                                      # finds ../../target/release automatically

cd ../otzaria_demo
dart pub get
dart run bin/otzaria_demo.dart build  --native <path to dll> [--db seforim.db] [--index dir] [--max-lines N] [--keep-marks] [--no-positions]
dart run bin/otzaria_demo.dart search --native <path to dll> [--index dir] [options] <query>
```

Library lookup:
- **Native engine:** the `--native` path, else the `SEFORIM_SEARCH_NATIVE` env var, else the OS loader search path. The file is `seforim_search_native.dll`, `libseforim_search_native.so` or `libseforim_search_native.dylib`. On iOS it is taken from the process.
- **libzstd:** the `--zstd` path, else the `ZSTD_LIB` env var, else the platform names. On Windows the last fallback is Git for Windows' `C:\Program Files\Git\mingw64\bin\libzstd.dll`.
- **DB:** `--db`, else `%APPDATA%\otzaria\books\seforim.db`, else Zayit's DB.
- **SQLite:** `package:sqlite3` 3.x bundles SQLite through build hooks, so no DLL is needed.

Run `search` with no options to see the full option list.

## API (`package:seforim_search`)

```dart
SeforimNative.load(path);                       // optional; otherwise loaded on first use
final index = SeforimIndex.create(dir, const IndexOptions(positions: true, keepMarks: false));
final w = index.openWriter(heapBytes: 50000000, threads: 0);
w.add(LineDocument(lineId:, position:, bookId:, text:));   // bare minimum: placement comes from SQLite
w.delete(lineId); w.commit('payload'); w.rollback(); w.mergeAndClose(); w.close();
index.buildFmSidecar();  // after mergeAndClose: FM-index for leading-* and fuzzy (index.hasFmSidecar)
index.info();                                   // IndexInfo(numLines, positions, keepMarks, payload)
SeforimIndex.exists(dir); SeforimIndex.open(dir); SeforimIndex.nativeVersion;
SeforimIndex.reducePairedReadings(query);        // (X) [Y] -> Y, the engine's own rule, before numbering words

// The public API: the query string (FtsLib syntax * ? % ~N | stays in it),
// options for every word, and per-word overrides (null = inherit).
final s = index.search(SearchOptions('תורה שלו* שבת~1 משה | אהרן',
    grammarPrefixes: true, prefixMaxLetters: 4,
    words: {0: WordOptions(ktivMaleHaser: true, alternatives: ['אורייתא'])},
    maxWordDistance: 5, requireOrdered: false, contextWords: 8));
// low-level form (slots built by hand): index.searchRequest(SearchRequest(groups: [...]))
s.hitCount; s.hits(offset, count); s.allHits();          // PlacedHit(position, lineId, bookId), catalog order
s.locateBatch(texts); s.locate(text);                    // Snippet? (null = filtered out)
s.countNeedsText; s.countBatch(texts); s.counts();       // exact totals: HitCounts(total, byBook, validated, exact)
index.search(options, cancel: t);                       // t = SearchCancelToken(); t.cancel() -> SearchCancelledException
s.takeNewTerms();                                        // term table, indexed by SnippetMark.termId
s.close(); index.close();
```

- **Offsets.** `Snippet.start`, `end` and the marks are UTF-16 offsets, so they index Dart strings directly.
- **Freeing.** Every handle (index, writer, search) has a `NativeFinalizer`. `close()` frees the handle and detaches the finalizer, so a handle is never freed twice, and calling `close()` again does nothing.
- **Lifetimes.** A writer or session keeps a reference to its index, so the index cannot be garbage-collected first. An explicit `index.close()` must still come last.
- **Errors.** Native failures throw `SeforimEngineException`, carrying the message from `se_last_error`.

## Otzaria → engine mapping

The current Otzaria app is [github.com/Otzaria/otzaria](https://github.com/Otzaria/otzaria). Its query building is in the Rust package `otzaria_search_engine`. Its Dart model is in `lib/search/models/search_configuration.dart` and `lib/search/search_query_builder.dart`. The mapping is in `otzaria_demo/lib/otzaria_query.dart`.

| Otzaria | Engine |
|---|---|
| `SearchMode.exact` | Only the 5 exact-mode options are kept: grammatical prefixes/suffixes, prefixes, suffixes, ktiv. |
| `SearchMode.advanced` | All options, plus alternatives. |
| global options / `wordOptions` | The global options become `SearchOptions`' own fields; a word whose options differ gets only the differing fields in `SearchOptions.words[i]` (`WordOptions`, null = inherit). The query goes as one string; the engine parses it with FtsLib's rules. |
| `SearchMode.fuzzy` | Each word sent as `word~N` (N 1..2) in the query string. |
| `negativeQuery` / `negativeScope` | `SearchOptions.exclude` (`ExcludeOptions`): the negative words as their own all-words query with the positive distance and order (as Otzaria passes `negativeDistance: distance`, `negativeScope: proximityScope`); scope paragraph = whole line without limits, scope section is host-side (the index holds no sections): one search per negative word, line ids → sections via SQLite `line_toc` (`SeforimDb.sectionsOf`), sets intersected (`blockedSections`), positive hits in those sections dropped (a hit without a section never), as Otzaria's `scoped_words_query`. Demo: `--exclude "words" [--exclude-scope distance|paragraph|section]`, global options only. |
| `wordMatchMode` / `wordMatchCount` | `SearchOptions.wordMatch` (`WordMatch.anyWord()` ...), passed as the user set it: the engine computes Otzaria's `min_words` (anyWord 1, mostWords n/2+1, atLeast N) over the slots it searches; distance, order and spacing are then not applied, as in Otzaria. Advanced mode only. |
| קידומות דקדוקיות / סיומות דקדוקיות | `grammarPrefixes` / `grammarSuffixes`. Prefixes are Otzaria's three slots (ו מ דא כש כ ב ש ל ה ד, then כש כ ב ש ל ה ד, then ה). As in Otzaria, a root under 3 letters takes none unless it is an acronym (has `"`): the engine's walk rule, per form. Suffixes are Otzaria's table, with final letter → base form (מלך → מלכים) and final ה → ת/ות (תורה → תורתו). |
| קידומות / סיומות | `advancedPrefixes` / `advancedSuffixes`, plus `prefixMaxLetters` / `suffixMaxLetters`. |
| כתיב מלא/חסר | `ktivMaleHaser` (Otzaria's י/ו rules: אותו ↛ אות, שבת ↛ שבות, מצות ↔ מצוות) |
| חלק ממילה | `partOfWord`, plus `partOfWordMaxLetters` |
| שגיאות כתיב | `spellingErrors` + `typoMode`: `TypoMode.otzaria` (default) = exactly Otzaria (edit distance 1 for a lone 4+-letter word with typo only; else its sampled list; 3 letters confusions only); `TypoMode.fullEdit1` = edit distance 1 in every combination. |
| קידומות / סיומות ארמיות | `aramaicPrefixes` / `aramaicSuffixes` |
| התעלם מגרשיים | `ignoreQuotes`. Without it a word typed with no quote still finds the quoted form (רמבם → רמב"ם), as Otzaria's quote-free twin, done in the walk; `exactQuotes` turns that off. |
| `grouping` sameSection / identicalText | Host-side (`lib/grouping.dart`), as Otzaria's `GroupCollector`: groups over the raw hits, section = `line_toc.tocEntryId` read from SQLite by line id (`SeforimDb.sectionsOf`; none = alone), identical text = Otzaria's `line_dedup_hash` (letters and digits, at least 12 Hebrew letters, else alone); groups in order of their first member, a page counts groups, up to 10 siblings per card; the representative is the first member whose snippet passes. Identical text reads up to `--group-scan` hits (default 20000) and marks counts as a lower bound past that. Demo: `--group section\|identical`. |
| ראשי תיבות | Host-side port of Otzaria's `AcronymLexicon` (`lib/acronym_lexicon.dart`) over `assets/Acronyms.json`, with its rule from `acronym_alternatives`: only when the whole query is one acronym (→ its multi-word expansions) or one known expansion (→ its acronyms). Sent as `SearchOptions.phraseAlternatives` (`PhraseOptions`). A decoded acronym gets ignore-gershayim (Otzaria's index has a quote-free twin token; ours keeps the quote). Demo: `--acronym --acronyms path`. Measured: רבי משה בן נחמן 557 → 16,781 lines. |
| תרגום ארמי | Host-side port of Otzaria's `TranslationLexicon` (`lib/translation_lexicon.dart`) over the app's `assets/dictionary.json`: both directions, single-word glosses only, cap 16, appended to the word's `WordOptions.alternatives`, as Otzaria's `translation_alternatives`. Demo: `--translation --dictionary path`. |
| ניקוד / טעמים (match the typed marks) | Needs an index built with `--keep-marks`. Without the option, the demo sets `ignoreNikud` / `ignoreTeamim` on such an index. |
| `distance` (gap), default scope "in order" | `SearchOptions.proximity = Proximity.ordered([distance, ...])`: the engine's own Otzaria proximity (each pair at most `distance` words apart, in order); positions index = span query, no-positions index = validated on the text by the locator, same lines. |
| `customSpacing` | `Proximity.ordered(gaps)`, gap i = `customSpacing[i]`, a missing pair = the largest custom value (Otzaria's `resolve_gaps`); any order is ignored with it, as in Otzaria. |
| word indexes, dropped words | Index i = the i-th word as typed (the demo's split words joined by spaces), so `wordOptions`, `alternativeWords` and the gaps are sent as the user set them. The engine maps an override to every part of a word its separators split, and cuts the words it cannot search (one letter, 30+ letters, no letters) out of the proximity, merging their gaps (`g0 + g1`; any order: window minus one per dropped word). |
| `anyOrder` | `Proximity.anyOrder(distance)`: all words inside one window of (k-1)(distance+1) positions, each at its own word (Otzaria's `Proximity::any_order`). |
| `alternativeWords[i]` | `SearchOptions.words[i].alternatives`. In the CLI, write `a,b`. |
| scope `sameParagraph` | Leave out `--distance` limits. A line is the paragraph unit. |
| `order: catalogue` | This is the only order. Positions come from the catalog-tree walk at build time. |
| book/category facets | `--book TEXT`: the book ids whose title or an ancestor category title (`category_closure`) matches go to the engine as `SearchOptions.books` (filtered search on the indexed `book_id`). |
| `limit` / `offset` | Counted over results that pass the snippet filters, as Otzaria's pages are. |
| query sanitizing | Same rules: ״→", ׳→', maqaf and `-` → space, an edge `"` is dropped, `''`→". |
| result `title` / `reference` / snippet | Book title, `line.heRef`, and the snippet with `⟦marks⟧`. Short windows (`isShort`) are located again over the line joined with ±2 neighbour lines, by the demo, which reads the hit's lineIndex from SQLite (`lineIndexesOf`; seforim.db knowledge stays out of the engine). |

### Gaps: what Otzaria does that the engine/demo can't (yet)

- **Scope `sameSection` for the positive query** (all words under one heading, spread over lines).
- **Relevance ordering.** Results come in catalog order only.
- **Regex queries** (`/…/`), the semantic and smart query modes.
- **Totals.** `hitCount` counts lines *before* the distance/order filter. The exact total and per-book counts
  come from `counts()`: exact at once when no text-level check applies (`countNeedsText` false), else after
  feeding every hit's text with `countBatch` (the demo's `--count`).
- **Exact-nikud matching** works only on a `--keep-marks` index. The default index strips marks.

## Demo specifics and simplifications

- **Catalog order.** Categories are sorted by `(orderIndex<0, orderIndex)`. Within a folder, books with `isBaseBook` come first, then subfolders, then the remaining books by `orderIndex`. The C# demo also matches base texts by normalized title (`SeforimDbBookTreeOrder` + `CatalogTextNormalizer`), which is not ported, so order can differ slightly for folders whose base book lacks `isBaseBook`.
- **Content decoding.** This follows CLAUDE.md:
  - The decision is made on the runtime type: `Uint8List` is a zstd frame, and a `String` starting with `KLUv` is tried as a base64 frame.
  - The dictionary is chosen by the ID embedded in the frame and dictionary headers.
  - Output is capped at 16 MB.
  - There is no viable pub.dev zstd package with dictionary support, so `lib/zstd.dart` binds `ZSTD_decompress_usingDDict` from the system libzstd.
  - Plain-text DBs, both schema < 6 and split-but-uncompressed, need no libzstd.
- **Index payload.** The build stores `{"lines":N,"db":"size|mtime"}` as the commit payload. Resuming an interrupted build is not implemented.
