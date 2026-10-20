# SeforimDB Search Engine

A Hebrew full-text search engine for the Otzaria / Zayit `seforim.db`, built on
tantivy and made to return **exactly what KitveiHakodesh's FtsLib returns**:
the same matches, catalog order, snippets and highlights. It adds Otzaria's
search options (prefixes, suffixes, ktiv, nikud, …) as fast walks over the
term dictionary.

Runs on any native platform (Windows, macOS, Linux, Android, iOS) through one
C library. From .NET it's a package that works on both .NET 10 and .NET
Framework 4.8.

## How it works

1. **Index.** One document per line. The line text is tokenized exactly as
   FtsLib's `HtmlWordScanner` does. The document also holds the line's catalog
   position, which is the order results come back in. No text is stored: the
   host app keeps the text in seforim.db.
2. **Search.** Each query word becomes one *walk* over tantivy's term
   dictionary (FST). The walk visits only spellings the word's options allow:
   prefixes, suffixes, wildcards, ktiv variants and so on. The words of a query
   are ANDed. Each walk reports the dictionary words it actually matched
   (`MatchedTermsSink`), so the search returns its hits **plus** the expanded
   words.
3. **Snippets.** For each hit, the host passes in the line text and the
   engine runs FtsLib's snippet locator, ported step for step:
   - find the tightest window covering every query word
   - add ±8 words of context and the ellipsis flags
   - mark every matched word inside the window
   - apply the word-distance and word-order filters
   - widen short windows over neighbouring lines of the same book

   The output is offsets only, not HTML, so the page renderer is unchanged.

## Layout

| Folder | What | Depends on |
|---|---|---|
| `tantivy/` | Our modified copy of tantivy 0.27.0 (the "KleiKodesh version"). Git submodule → github.com/KleiKodesh/tantivy (separate repo, plain commits on `main`). After tantivy commits, `git add tantivy` here to move the pointer. Clone with `--recurse-submodules`. | — |
| `hebrew-fts/` | Hebrew layer: tokenizer, walks, wildcard, snippet locator. | tantivy |
| `seforim-engine/` | The engine. **No database code**: the host feeds lines and texts. | hebrew-fts |
| `engine-ffi/` | C ABI → `seforim_search_native.dll / .so / .dylib`. | seforim-engine |
| `csharp/SeforimSearch.Engine/` | .NET API package (netstandard2.0 → .NET 10 and .NET Framework 4.8). | native lib |
| `csharp/SeforimSearch.Demo/` | Everything that reads seforim.db, plus an FtsLib-shaped facade. net10.0 and net48. | Engine package, SQLite, zstd |

## Build and run

```
cargo build --release -p engine-ffi                  # native library
dotnet build csharp/SeforimSearch.Demo -c Release    # both net10.0 and net48

SeforimSearch.Demo build  [--db path] [--index dir] [--max-lines N] [--no-positions]
SeforimSearch.Demo search [--db path] [--index dir] [--prefixes] [--suffixes] [--distance N] <query>
```

Tests: `cargo test` at the root (hebrew-fts, seforim-engine, engine-ffi), and
`cargo test --lib query::` in `tantivy/`.

Measured on the real Otzaria DB (7.08M lines), first 200k lines:
- Building takes ~12 s through the C# feed, so a full build is roughly 5–7 min.
- Searching תורה with prefixes returns 2,002 hits with snippets, identical on
  .NET 10 and .NET Framework 4.8.

## What's in each part

### tantivy (our changes)
- **MatchedTermsSink**, opt-in: a search also returns the words its FST
  expansion matched. It costs nothing when off.
- **Expansion cap inside the walk.** Every walk except fuzzy is capped at 2000
  terms, and the cap is applied inside the walk itself:
  - The term dictionary is walked shortest word first, level by level, across
    all index segments together, and the walk stops at the cap (ties go to the
    more frequent word).
  - So `*word` always finds `word` first, then the shortest affixes, and the
    long tail is never visited.
  - Fuzzy is the exception: it walks fully and keeps the closest matches
    (distance-ranked).
  - Broad queries are capped rather than failing.
- **SpanQuery**: word distance where each query word can be any walk, with a
  separate limit per gap (`a –2– b –5– c`). Word order can be required or not.

### hebrew-fts
- **Tokenizer.** A port of FtsLib's `HtmlWordScanner`:
  - HTML-aware: inline tags are transparent; block tags, entities and maqaf
    split words.
  - Only words of 2–29 letters are indexed.
  - Intra-word quotes are kept as `"`.
  - Two variants: `hebrew` strips nikud and te'amim, `hebrew_marked` keeps
    them.
- **Walks:**
  - **Grammar** (`%word`, `word%`): 34 prefixes and 32 suffixes.
  - **Otzaria options:** grammatical/Aramaic prefixes and suffixes, ktiv
    male/haser, part-of-word, advanced prefixes/suffixes, ignore gershayim,
    spelling/OCR errors, ignore nikud, ignore te'amim.
  - **Length limits are set by the caller** (Otzaria), as plain ints; unset
    means the default:

    | Option | Default |
    |---|---|
    | `PrefixMaxLetters` | 3 (5 for a word of ≤3 letters) |
    | `SuffixMaxLetters` | 5 (7 for a short word) |
    | `PartOfWordMaxLetters` | 3 per side (2 for 4+-letter words) |
    | `GrammarPrefixMaxLetters` | 3 |
  - **Wildcard** (`*`, `?`) with FtsLib's limits:
    - at least 2 real letters, at most 4 `?`
    - `*` may cover 3 characters before the anchor, 4 after it, or 7 in total
      when both ends are wildcards
- **Snippet locator.** A step-for-step port of FtsLib's
  `PreparedQueryGroups`, `SnippetWindowLocator` and `SnippetDescriptor`.
  Offsets are UTF-16 (.NET/JS string indices).

### seforim-engine / engine-ffi
- **Index:** create or open; positions optional; add/delete/commit lines with a
  host payload (resume watermark, provenance stamp) committed atomically with
  the documents.
- **Search:** every matching line in catalog order, plus a snippet session.
  - The session locates snippets for a whole batch across all cores.
  - It applies the word-distance and order filters and widens short windows
    over neighbouring lines.
  - It keeps the term table.
- **Query groups:** word (with options), wildcard, fuzzy (distance ≤2), and OR
  of alternatives.
- **C ABI conventions:** objects are opaque handles; strings cross as UTF-8
  pointer + length; errors are `0/-1` status codes with a per-thread
  `se_last_error`; hot paths are binary (no JSON).

### C# demo (`csharp/SeforimSearch.Demo`)
- **DB code**, copied from KitveiHakodesh Core with namespace renames only:
  - schema probe (3 DB shapes), zstd codec, catalog tree order, content
    fingerprint
  - corpus reads, and `SeforimDbFtsSearcher` (batch plan, neighbour widening,
    seeded line texts)
- **FtsLib facade with the same names:** `FtsDocumentPlacement`,
  `SearchResult`, `SnippetDescriptor`, `QueryParser` (`%` `~` `*` `?` `|`), and
  `TantivySeforimIndex` (`Search`, `LocateSnippetWindow`,
  `LocateSnippetWindowOverNeighbourLines`, resumable `BuildLines`,
  `GetResumeState`).

## Differences from FtsLib (known, deliberate)
- **Fuzzy distance** is capped at 2 (tantivy's limit; FtsLib allowed 3).
- **Quotes:** a plain query word doesn't match a quoted word unless "ignore
  gershayim" is on (רשי ↛ רש"י). FtsLib always matched.
- **Ketiv/qere expansion** (`expandKetiv`) is replaced by the Otzaria walk
  options.
- **Order and distance:** results are in catalog order, as FtsLib. Word
  distance is checked from the line text, so an index without positions works.

## Open
- Dart binding over the same C ABI.
- A full build compared side by side with FtsLib on the same queries.
- Profiling: C# search is ~1.0 s (.NET 10) / ~1.5 s (net48), against 378 ms in
  the pure-Rust smoke test for the same query. That gap includes process start
  and index open; not profiled yet.
- Choosing span query vs post-filter for very broad expansions (needs a
  benchmark).
