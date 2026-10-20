# SeforimDB Search Engine

A Hebrew full-text search engine for the Otzaria / Zayit `seforim.db`, built on
tantivy and made to return **exactly what KitveiHakodesh's FtsLib returns**:
the same matches, catalog order, snippets and highlights. It adds Otzaria's
search options (prefixes, suffixes, ktiv, nikud, …) as fast walks over the
term dictionary.

Runs on any native platform (Windows, macOS, Linux, Android, iOS) through one
C library. From .NET it's a package that works on both .NET 10 and .NET
Framework 4.8.

## Purpose

This project has two goals: to build a new and improved search engine for
Otzaria, and to unify the search engine of the KleiKodesh project
(KitveiHakodesh) with the Otzaria project, so one engine serves both apps.
Today KitveiHakodesh searches with FtsLib (SQLite FTS5 + C#) and Otzaria has
its own search. This engine is built to return exactly FtsLib's results, so
KitveiHakodesh can switch to it, while adding Otzaria's search options as fast
walks over the term dictionary, so Otzaria can switch to it too. The outcome
is one engine, one index format and the same results for both apps.

## The main changes (what makes this engine different)

1. **Custom FST walks.** Every search option (Hebrew/Aramaic prefixes and
   suffixes, ktiv male/haser, part-of-word, wildcards, nikud/te'amim, spelling
   errors, …) is an automaton walked directly over tantivy's term dictionary.
   The walk visits only the spellings the options allow, shortest first, and
   stops at the expansion cap. No candidate lists, no per-candidate lookups.
2. **Matched FST terms come back with the search.** The words a walk actually
   matched in the dictionary are returned together with the hits
   (`MatchedTermsSink`), in the same pass. No separate query is needed to
   learn which words matched; the snippet/highlight stage uses them directly.
3. **Snippets.** For each hit, the host passes in the line text and the
   engine runs FtsLib's snippet locator, ported step for step:
   - find the tightest window covering every query word
   - add ±8 words of context and the ellipsis flags
   - mark every matched word inside the window
   - apply the word-distance and word-order filters
   - widen short windows over neighbouring lines of the same book

   The output is offsets only, not HTML, so the page renderer is unchanged.

Everything else (tokenizer, catalog order) is a faithful port of FtsLib, so
results match what KitveiHakodesh shows today.

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
   engine runs FtsLib's snippet locator (see main change 3 above), returning
   offsets only.

## Layout

| Folder | What | Depends on |
|---|---|---|
| `tantivy/` | Our modified copy of tantivy 0.27.0 (the "KleiKodesh version"). Git submodule → github.com/KleiKodesh/tantivy (separate repo, plain commits on `main`). After tantivy commits, `git add tantivy` here to move the pointer. Clone with `--recurse-submodules`. | — |
| `hebrew-fts/` | Hebrew layer: tokenizer, walks, wildcard, snippet locator. | tantivy |
| `seforim-engine/` | The engine. **No database code**: the host feeds lines and texts. | hebrew-fts |
| `engine-ffi/` | C ABI → `seforim_search_native.dll / .so / .dylib`. | seforim-engine |
| `csharp/SeforimSearch.Engine/` | .NET API package (netstandard2.0 → .NET 10 and .NET Framework 4.8). | native lib |
| `dart/seforim_search/` | Dart package over the native library (`dart:ffi`, finalizers). 6 tests. | native lib |
| `dart/otzaria_demo/` | Dart CLI doing Otzaria's search on this engine: reads seforim.db (`sqlite3`, system libzstd), maps Otzaria's options to walks. See `dart/README.md` for gaps. | Dart package |
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

### tantivy: the changes made to tantivy itself, and why

Our tantivy copy is upstream 0.27.0 (commit `67889269`) plus the changes below,
all in `src/query/` and `src/termdict/`. Every change is opt-in: a query that
doesn't use it runs upstream's exact code path, at no cost.

The changes are kept **ready for an upstream pull request**, and what we run
is that same code: nothing Hebrew or FtsLib-specific lives in tantivy (the
quote rule is passed in by hebrew-fts), both term dictionaries (FST and
quickwit's SSTable) are supported, upstream's tests are untouched, rustfmt
is clean, and CHANGELOG.md has the entry.

1. **MatchedTermsSink: return the matched words with the search.**
   (`query/automaton_weight.rs`, plus `fuzzy_query.rs` and `regex_query.rs`
   to attach a sink)
   - *What:* a sink attached to a fuzzy/regex/any-automaton query collects
     every dictionary word the walk matched, during the search.
   - *Why:* tantivy walks the FST to expand the query and already has each
     matched word, but upstream discards them (`without_keys`). The snippet
     and highlight stage needs exactly those words (FtsLib's
     PreparedQueryGroups). Keeping them costs one copy per matched word,
     outside the per-document loop, and avoids a second query or re-walk to
     learn them.

2. **Shortest-first term dictionary walk.**
   (`termdict/fst_termdict/shortest_first.rs`, `termdict/mod.rs`)
   - *What:* a breadth-first walk of the FST by word length in characters.
     All matches of length L come before any of length L+1, and the caller
     stops pulling levels when it has enough. The caller may name ASCII
     bytes that count as zero width (`ZeroWidthBytes`); hebrew-fts passes
     the intra-word quote, so רש"י is as short as רשי, as in FtsLib whose
     terms carried no quotes. With quickwit's SSTable dictionary (no FST to
     walk) the same API streams the whole expansion once and groups it by
     length: same result, full-walk cost.
   - *Why:* upstream walks matches in alphabetical order. To keep the best
     N, it would have to walk the whole expansion and then cut. This walk
     makes the cap work inside the walk: the bare word first, then the
     shortest affixes, and the long tail of a broad pattern is never
     visited.

3. **Expansion cap instead of errors.**
   (`query/automaton_weight.rs`: `set_max_expansions`,
   `set_expansion_rank`, `set_zero_width_bytes`; `fuzzy_query.rs`,
   `regex_query.rs`, `phrase_query/regex_phrase_weight.rs`)
   - *What:* an automaton query can be capped at N words (we use FtsLib's
     2000). Unranked queries use the shortest-first walk, with ties going to
     the more frequent word. Fuzzy is ranked by edit distance first.
   - *Why:* upstream either expands without limit or fails the whole query
     ("exceeded max expansions"). FtsLib caps broad patterns and still
     returns results, and so must we.

4. **Cap per segment.** (`query/automaton_weight.rs`)
   - *What:* the cap is chosen in each segment's dictionary separately, and
     the walk stops at the cap there.
   - *Why:* it's simple and keeps all work inside each segment. Our builds
     merge the index into one segment, where per-segment equals a single
     global choice, as in FtsLib's one term index. After incremental updates
     add segments, merge again to keep that.

5. **SpanQuery: per-pair word distance over any walk.**
   (`query/span_query.rs`, new)
   - *What:* "word A, then within N words word B, then within M words word
     C". Each gap has its own limit, gaps count words between, order is
     optional, and one word never fills two slots. Each slot can be any term
     source: an exact word, fuzzy, regex, or any of our Hebrew walks.
   - *Why:* upstream's PhraseQuery has one shared slop budget and plain
     terms only (RegexPhraseQuery: regex only). Otzaria's custom per-pair
     spacing over expanded words needs both.

Also small: `get_match_term_infos` honours the cap and the sink, and
`RegexPhraseWeight` is reachable crate-internally so SpanQuery can reuse its
positioned union. `FuzzyTermQuery`'s Debug text is unchanged (the new fields
are not printed), so upstream's query-parser tests stay as they are.

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
- **Query groups:** word (with options), wildcard, grammar (`%`), fuzzy
  (distance ≤2), and OR of alternatives.
- **Fuzzy** is hebrew-fts's own `HebrewFuzzyQuery` (same Levenshtein DFA as
  tantivy's, which cannot be built from outside tantivy): cap 2000 per
  segment, closest edit distance first, then shortest with the quote zero
  width, then most frequent. The plain query and the span slot are the same
  weight.
- **Per-pair spacing** (Otzaria's customSpacing): `gaps` in the request (C#
  `SearchRequest.Gaps`, Dart `SearchRequest.gaps`, demo `--spacing 2,5`).
  - `gaps[i]` is the maximum number of words between query word i and i+1.
  - Every group kind works as a slot; word order follows `RequireOrdered`.
  - It runs as a span query, so it needs an index with positions; otherwise
    the engine returns a clear error.
  - Measured on the full index, בית המקדש adjacent: 15,218 lines in 29 ms.
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

## Parity with FtsLib (measured)

`tools/parity/` runs KitveiHakodesh Core's own searcher on the installed FtsLib
index (`FtsLibDump`) and our demo (`SeforimSearch.Demo dump`) on the same DB
and query list, then diffs every hit with `compare.py`. The diff covers hit
order, word distance, snippet window, snippet lines, flags, and each mark's
offsets and term.

On the full Otzaria DB (7,080,797 lines), first 2,000 hits per query: **16 of
17 queries identical.** That covers plain words, multi-word, `%`, `*`, `?`,
`|`, and distance 0/2/5. The one difference is `word~N`, where 2 hits differ
because the fuzzy DFA counts an intra-word quote as an edit.

Full build: 5 min plus a 70 s merge into one segment. The index is 1.6 GB with
positions.

## Differences from FtsLib (known, deliberate)
- **Fuzzy distance** is capped at 2 (tantivy's limit; FtsLib allowed 3).
- **Quotes:** the index keeps intra-word quotes (רש"י) where FtsLib dropped
  them. Matching ignores them through the "ignore gershayim" option, which the
  FtsLib facade turns on by default, so results match FtsLib. A quote after
  the last letter is punctuation and is never part of a term.
- **Mark spans can contain tags.** A word that ends at a block tag (heading,
  `<br>`, …) has its mark run up to that tag's `>`, e.g. `בראשית</h1`. This is
  FtsLib's span exactly: its scanner flushes the word at the `>`. Renderers
  must strip tags inside a mark, as the KitveiHakodesh page does.
- **`%word`** uses FtsLib's GrammarExpander tables (34 prefixes / 32
  suffixes). The Otzaria prefix/suffix options are separate walk options.
- **Ketiv/qere expansion** (`expandKetiv`) is replaced by the Otzaria walk
  options.
- **Order and distance:** results are in catalog order, as FtsLib. Word
  distance is checked from the line text, so an index without positions works.

## Open
- Otzaria gaps found by the Dart demo (see `dart/README.md`):
  - תרגום ארמי and ראשי תיבות have no walk.
  - The "most words" / "at least N" modes, exclusion words, and grouping are
    missing.
- Leading-`*` speed: a trigram side index for `*x*` (`*יצח*` 1.1 s vs FtsLib
  0.26 s).
- A full build compared side by side with FtsLib on the same queries.
- Profiling: C# search is ~1.0 s (.NET 10) / ~1.5 s (net48), against 378 ms in
  the pure-Rust smoke test for the same query. That gap includes process start
  and index open; not profiled yet.
- Choosing span query vs post-filter for very broad expansions (needs a
  benchmark).
