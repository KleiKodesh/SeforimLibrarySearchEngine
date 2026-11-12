# SeforimDB Search Engine

A Hebrew full-text search engine for the Otzaria / Zayit `seforim.db`, built on
tantivy and made to return **exactly what KitveiHakodesh's FtsLib returns**:
the same matches, catalog order, snippets and highlights. It adds Otzaria's
search options (prefixes, suffixes, ktiv, nikud, …) as fast walks over the
term dictionary.

Runs on any native platform (Windows, macOS, Linux, Android, iOS) through one
C library. From .NET it's a package that works on both .NET 10 and .NET
Framework 4.8.

## Rule: the index is the bare minimum (engine + seforim.db in tandem)

The engine always works **together with the seforim.db SQLite database**, so
the index stores only what the engine itself needs:

- the postings (with freqs/positions only when the index is built with
  positions, for span/slop queries)
- `line_id`, the catalog `position`, and `book_id` (kept for **filtered
  search by book**)
- on an index built with `keep_marks` only: a `marked` flag on the lines
  that carry a nikud / ta'am mark (one term; Otzaria's vocalized search
  sees only those lines, so the engine filters by it)
- small per-line fast fields, read only when their option is used
  (user's decision): the line's **content edges** `line_first` /
  `line_last` (position + 1 of its first / last content word, skipping a
  leading or trailing enumerator such as `(x)`, `{x}`, `(49)`: Otzaria's
  `lineFirst` / `lineLast`; 0 = no content word; cross-line matching; the
  engine computes them with the scanner that numbers the positions), its
  **line flags** (bit 0: no phrase continues into or out of the line, the
  host sets it for headings; bits 1 / 2: the line starts / ends with an
  enumerator; bit 3: the line holds a word too long to be a term, so a line
  regex always reads it) and its **section id** (same-section scope; the host's, from
  seforim.db)

Nothing else: no text, and no data the host can read from SQLite by line id.
Grouping, sections, line index, references, neighbour lines and the like are
done by the **host** with SQLite on the returned line ids (one indexed
`IN (...)` query per batch). This applies to the C# and Dart hosts alike, and
any similar future need follows the same rule.

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
   - find the tightest window covering every query word; with the "in
     order" option, the tightest window holding them IN QUERY ORDER (FtsLib
     too), and the order filter is that window's existence
     (so with order AND a word distance, the distance is measured on the
     ordered window: a line whose only close pair is out of order no longer
     passes; FtsLib changed the same way, so parity holds)
   - add ±8 words of context and the ellipsis flags
   - mark the words of every full occurrence inside the window (issue #4):
     with a word distance (FtsLib) or Otzaria's proximity (ordered gaps / any
     order) only the tokens of occurrences that satisfy the rule, every such
     occurrence; across a line break only the occurrence that crosses it;
     without a distance, and with partial word match, every query word in the
     window (as before); in order, only the tokens of ordered occurrences
     (within the distance, if any). FtsLib marks the same way (its distance
     filter)
   - apply the word-distance and word-order filters
   - report the window's word count, so the host can flag a short window
     (FtsLib's `SnippetFlagShort`, fewer words than the context). The window
     is located ONCE, over the matched line alone, and is the hit's final
     descriptor. A short row is shown with its same-book ±2 non-empty
     neighbour lines as plain context, never located again (KitveiHakodesh
     5e1091b8; what counts as a neighbour is seforim.db knowledge, so the
     host reads it: `FetchSnippetTexts`, one SQL statement per batch)

   The output is offsets only, not HTML, so the page renderer is unchanged.

Everything else (tokenizer, catalog order) is a faithful port of FtsLib, so
results match what KitveiHakodesh shows today.

<!-- SPECS:BEGIN (generated by tools/report/specs_report.py) -->
## Specs

_Generated by `tools/report/specs_report.py` from the same data as [FTS-SPECS-AND-COMPARISONS.md](FTS-SPECS-AND-COMPARISONS.md) (full charts, per-query stages, hit-count comparison, first-page profile). Index formats: ours `tantivy16`, FtsLib `fts13`._

**Stale (full-DB rerun pending):** Index builds, parity and search speed below were measured on `tantivy9` / FtsLib `fts8`. `tantivy10` / `fts9` changed indexing (an unterminated `<` no longer swallows the rest of the line; per-line content edges for cross-line) and fuzzy (a swap of two adjacent letters is one edit): the full-DB rebuilds, parity sets and the Otzaria hit-count comparison need a rerun. `tantivy11` sorts the index by catalog position (search skips its sort on the merged segment) and the C# / Dart hosts keep seforim.db open across searches: index build time and size, and the first-page profile, need a full-DB rerun. `tantivy12` / `fts10`: a `<sup>` holding a vocalized Hebrew letter mid-word is transparent (issue #2 H58), and a keep_marks term drops U+034F (combining grapheme joiner, issue #2 H02): both full-DB rebuilds and the parity sets need a rerun. `tantivy13` / `fts11`: catalog order (= position / rowid) counts a base text by the `isBaseBook` flag only (the title-equals-folder rule is gone, issue #6), so a few books move: both full-DB rebuilds and the parity sets need a rerun. In order + word distance (issue #4 follow-up): the distance is read from the tightest ORDERED window, so a line whose only close pair is out of order drops; snippet windows and marks follow the order: the parity sets with in-order queries need a rerun (both engines changed together). `tantivy14` / `fts12`: schema-7 footnotes (`line_note`) are indexed as a virtual notes book right after each book with notes (issue #13; about 263k notes in 614 books on the local DB): positions, line counts, both full-DB rebuilds, the parity sets and the Otzaria hit-count comparison need a rerun. `tantivy16` / `fts13`: the notes ids are 64-bit and JS-safe (R = 2^52, line = R + (lineId << 16) + noteIndex, issue #13): only the virtual ids change, the same full-DB reruns cover it. Issue #21: on an index with positions the hits are decided from the positions during the search and the snippet window is the word-tightest one (0.1-3% of the hits show another occurrence than FtsLib, a named deviation): the positions-flavour parity sets need a rerun (snippet differences expected), the no-positions flavour is unchanged. Hit-count comparison: the rows classified as our bugs (unterminated `<`, cross-line enumerators) and the fuzzy transposition rows are expected to change; not rerun yet.

| Feature | Status |
|---|---|
| FtsLib query syntax (`* ? % ~N \|`), results identical to FtsLib | done, parity below |
| Every option one FST walk per word, shortest first, cap per request (default 2000, 0 = none) across all segments, exact word kept, matched terms returned | done |
| Otzaria word options (grammar/advanced/Aramaic prefixes+suffixes, ktiv, part of word, typo modes, quotes) | done |
| Ketiv rule sets: Otzaria's or FtsLib's KetivExpander (`ktiv_rules`) | done |
| One search options object, per-word options (negative words too), alternatives, acronyms | done |
| Otzaria proximity (per-pair gaps, any order), word match (all/any/most/at least N) | done |
| Otzaria tokenizer rules (one-letter words, digits, joiners, geresh, paired readings, 128-byte limit) in both engines | done |
| Vocalized matching (nikud / te'amim) as a walk on a keep-marks index (plain, wildcard and fuzzy words) | done |
| Exact totals + per-book counts, cancellation, pull streaming | done |
| Hits decided from positions during the search, streamed in batches (issue #21): exact totals with no text, word-tightest window | done |
| Non-blocking search in the packages (Dart worker isolate `SearchWorker`, C# `SearchAsync` / `IAsyncEnumerable` batches) | done |
| Packaging (issue #17): C ABI version check at load (`se_abi_version`), Dart Native Assets hook (SHA-256 pinned prebuilt download / cargo / local file), NuGet RIDs win/linux/osx x64+arm64, release workflow + sync check | done |
| Resume after a catalog position (FtsLib's `afterPosition`; C# demo `PositionOfLine`, Core's `StreamSearch` with `Restarted`) | done |
| User regex `/.../` (word, sequence, line kinds) | done |
| Relevance / generation order (option; catalog default) + notice when positions are needed | done |
| Truncated flag when a cap is hit | done |
| Locate match mode: best window (default) or ALL MATCHES (whole text, every full occurrence marked, session locates; Core passes it through); find in an open text without an index (`se_find_all_matches`: words or phrase, quotes / marks options; both engines) | done; unit tests |
| Footnotes (schema 7 `line_note`) as a virtual notes book after each book; ids in a reserved 64-bit range (2^52+, JS-safe), hit maps to the source line (both engines) | done; unit tests; full-DB rerun pending |
| Cross-line phrases (one break, any search type; line-edge enumerators skipped as Otzaria) and same-section scope | built; unit tests; full-DB verification pending |
| Fuzzy: a swap of two adjacent letters is one edit (Otzaria; FtsLib changed too) | done |
| Quoted phrases (Otzaria's rule, both engines): a whole-query phrase is literal and may cross a line (ours); a phrase among words is required in the line; in lexical fuzzy mode each quoted word keeps its slot (fuzzy, forms), in order, at most 1 word between | done; unit tests; FtsLib per line only |
| Snippets from index positions (`locate_lines`, per-batch cost rule) | done |
| Morphology (Otzaria's MagicDictionary) as host lexicon: dictionary forms as `lexical_forms` (lexical tier in relevance), blacklisted forms searched but not marked (`unmarked_alternatives` / `unmarked_lexical_forms`), per-word cap | done (Dart demo host); unit tests |
| Negative query defaults to the positive proximity (Otzaria's negativeDistance; `proximity_base` under a custom spacing) | done |

**Index builds (full DB, 7,080,797 lines):**

| Build | Total | Size |
|---|---|---|
| ours, positions | 8.28 min | 1,646.1 MB |
| ours, no positions | 6.24 min | 748.6 MB |
| ours, positions + keep marks | 8.46 min | 1,696.9 MB |
| FtsLib (fts8) | 16.2 min | 746.0 MB |
| Otzaria v5.0.0 | 8.1 min | 1,914 MB |

**Parity with FtsLib** (identical lines, first 20,000 hits, both flavours): diff10 10/10, large 61/61, words 14/14, rules 21/21, ketiv 12/12.

**Search speed** (every hit read, median per query):

| Set | Per engine |
|---|---|
| queries-large.txt (61) | ours, positions: median 0.10 min, first batch 262 ms; ours, no positions: median 0.10 min, first batch 264 ms; FtsLib: median 0.10 min, first batch 274 ms; Otzaria: median 0.11 min, first batch 106 ms |
| queries-words.txt (14) | ours, positions: median 0.02 min, first batch 238 ms; ours, no positions: median 0.02 min, first batch 258 ms; FtsLib: median 0.03 min, first batch 261 ms; Otzaria: median 0.03 min, first batch 308 ms |
| queries-rules.txt (21) | ours, positions: median 0.03 min, first batch 257 ms; ours, no positions: median 0.03 min, first batch 246 ms; FtsLib: median 0.04 min, first batch 259 ms; Otzaria: median 688 ms, first batch 79 ms |

<!-- SPECS:END -->

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
| `seforim-engine/` | The engine. **No database code**: the host feeds lines and texts. Includes the FM-index sidecar (FtsLib's `FmIndex`) for leading-`*` and fuzzy. | hebrew-fts |
| `engine-ffi/` | C ABI → `seforim_search_native.dll / .so / .dylib`. | seforim-engine |
| `tools/release/`, `.github/workflows/` | Release targets, hash pinning, sync check, third-party notices; release + CI workflows. | — |
| `csharp/SeforimSearch.Engine/` | .NET API package (netstandard2.0 → .NET 10 and .NET Framework 4.8). | native lib |
| `dart/seforim_search/` | Dart package over the native library (`dart:ffi`, finalizers); Native Assets build hook (`hook/build.dart`). | native lib |
| `dart/otzaria_demo/` | Dart CLI doing Otzaria's search on this engine: reads seforim.db (`sqlite3`, system libzstd), maps Otzaria's options to walks. See `dart/README.md` for gaps. | Dart package |
| `csharp/SeforimSearch.Demo/` | Everything that reads seforim.db, plus an FtsLib-shaped facade. net10.0 and net48. | Engine package, SQLite, zstd |

## Build and run

```
cargo build --release -p engine-ffi                  # native library
dotnet build csharp/SeforimSearch.Demo -c Release    # both net10.0 and net48

SeforimSearch.Demo build  [--db path] [--index dir] [--max-lines N] [--no-positions] [--keep-marks]
SeforimSearch.Demo search [--db path] [--index dir] [--prefixes] [--suffixes] [--distance N] <query>
```

Release profile (workspace `Cargo.toml`): fat LTO, `codegen-units = 1`, `strip = "symbols"`.
Measured win-x64: `seforim_search_native.dll` 8.92 MB -> 8.35 MB, locate_bench / find_all_bench
same or faster (find_all one word best 24.1 -> 21.3 ms); release build ~7.4 -> ~13.5 min from clean.
Rejected: opt-level "s" (7.22 MB, find_all ~23% slower); panic = "abort" (engine-ffi catches panics
at the C boundary).

Tests: `cargo test` at the root (hebrew-fts, seforim-engine, engine-ffi), and
`cargo test --lib query::` in `tantivy/`.

Measured on the real Otzaria DB (7.08M lines), first 200k lines:
- Building takes ~12 s through the C# feed, so a full build is roughly 5–7 min.
- Searching תורה with prefixes returns 2,002 hits with snippets, identical on
  .NET 10 and .NET Framework 4.8.

## Packaging and releases (issue #17)

**C ABI version.** `se_abi_version()` returns engine-ffi's `SE_ABI_VERSION`
(`se_version()` stays, for diagnostics). **Bump rule: every change to the C
ABI bumps it**: a function added, removed or renamed, a signature, a struct
layout, a JSON shape or an enum / flag value a host reads. Bump the three
copies together (engine-ffi `SE_ABI_VERSION`, Dart `seforimAbiVersion`, C#
`NativeAbi.Expected`). Both packages check the loaded library at load and
refuse a mismatch with an error naming both versions and the library's path
(`se_native_library_path()`): Dart `NativeAbiMismatchException`, C#
`SeforimEngineException` from the first `SeforimIndex` / `TextMatcher` call.

**Dart package: Native Assets build hook** (`dart/seforim_search/hook/build.dart`,
`hooks` 2.2 / `code_assets` 2.1, Dart >= 3.10; the pattern of `package:sqlite3`
v3). The app picks the source in ITS root `pubspec.yaml`:

```yaml
hooks:
  user_defines:
    seforim_search:
      source: prebuilt            # default
      # url_pattern: https://mirror.example/{tag}/{file}   # optional mirror
      # source: cargo             # build from a checkout of this repository
      # source: file              # offline: a library on disk
      # path: native/seforim_search_native.dll            # string, or a map per OS name
      # sha256: <hex>             # optional check for source: file
```

- `prebuilt`: downloads `seforim_search_native-<os>-<arch>.<ext>` from this
  repository's GitHub release `v<package version>` and verifies its SHA-256
  against `lib/src/hook/asset_hashes.g.dart` (generated, pinned in the package);
  a mismatch is refused. When no hash is pinned for the target (or the pinned
  release is not this version) the hook builds with cargo if cargo and the Rust
  workspace are present, otherwise it fails naming the target and the release.
- `cargo`: `cargo build --release -p engine-ffi` (with `--target` when cross
  compiling; Android / iOS need the usual cargo linker setup). This package's
  own pubspec selects it, so `dart test` in the repository builds the library.
- `file`: a library on disk, path relative to the pubspec.
- `SEFORIM_SEARCH_NATIVE` (environment) and `SeforimNative.load(path)` still
  override everything (dev).
- Targets: windows / linux / macos x64 + arm64 (required in every release);
  android arm64 / x64 / arm and iOS device arm64 + simulator arm64 / x64 as
  dynamic libraries (best effort in the release workflow; iOS is bundled by
  Flutter as a framework).

**C# package**: `runtimes/<rid>/native/` for win-x64, win-arm64, linux-x64,
linux-arm64, osx-x64, osx-arm64, packed from `native-artifacts/<rid>/` (filled
by the release workflow). A dev build still copies `target/release`'s library
next to the app (.NET Framework 4.8).

**Always in sync** (`tools/release/check_sync.py`, CI on every push / PR,
`.github/workflows/ci.yml`): one version in engine-ffi `Cargo.toml`, the Dart
pubspec and the csproj; one ABI version in the three copies; the pinned hashes
file names no release newer than the version and, for this version, every
required target. `Cargo.lock` is committed and release builds use `--locked`.
`THIRD-PARTY-NOTICES.md` (every Rust crate in the library, with its license)
is generated by `tools/release/third_party.py`; CI fails when it is stale.
License: MIT (`LICENSE`, as every crate's `Cargo.toml` declares).

**Releasing** (the maintainer decides when; nothing publishes without a tag):
1. Bump the version in engine-ffi `Cargo.toml`, `dart/seforim_search/pubspec.yaml`
   and `SeforimSearch.Engine.csproj` (and the ABI version if the C ABI changed);
   run `python -I -X utf8 tools/release/check_sync.py` and
   `python -I -X utf8 tools/release/third_party.py`; commit, push.
2. `git tag vX.Y.Z && git push origin vX.Y.Z`. `.github/workflows/release.yml`
   builds every target, creates the GitHub release with the libraries,
   `SHA256SUMS`, `asset_hashes.g.dart` and the `.nupkg`, and opens a pull
   request "Pin native library hashes for vX.Y.Z" (needs the repository setting
   Actions > "Allow GitHub Actions to create and approve pull requests"; a pull
   request opened by the workflow token does not start CI by itself, so run
   `check_sync.py --pinned` locally or re-run CI on it). Re-running the workflow
   replaces the release assets and the pin branch.
3. Merge that pull request. Dart apps depend on a commit that has the pinned
   hashes (the merge commit, or a later one); the NuGet package from the
   release can be pushed to a feed by hand.

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
   - The sink also says when a capped expansion left terms out
     (`is_truncated`; `mark_truncated` for callers that cap an expansion
     themselves), at no cost unless a cap is hit.

2. **Shortest-first term dictionary walk.**
   (`termdict/fst_termdict/shortest_first.rs`, `termdict/mod.rs`)
   - *What:* a breadth-first walk of the FST by word length in characters.
     All matches of length L come before any of length L+1, and the caller
     stops pulling levels when it has enough. The caller may name
     characters that count as zero width (`ZeroWidthChars`: ASCII bytes
     plus ranges of multi-byte characters); hebrew-fts passes the
     intra-word quote, and the marks on an index that keeps them, so רש"י is as short as רשי, as in FtsLib whose
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
   `set_expansion_rank`, `set_zero_width_chars`; `fuzzy_query.rs`,
   `regex_query.rs`, `phrase_query/regex_phrase_weight.rs`)
   - *What:* an automaton query can be capped at N words (we use FtsLib's
     2000). Unranked queries use the shortest-first walk, with ties going to
     the more frequent word. Fuzzy is ranked by edit distance first.
   - *Why:* upstream either expands without limit or fails the whole query
     ("exceeded max expansions"). FtsLib caps broad patterns and still
     returns results, and so must we.

4. **One cap across segments, pinned terms.** (`query/automaton_weight.rs`:
   `set_cap_across_segments`, `set_pinned_terms`)
   - *What:* with `set_cap_across_segments(searcher.segment_readers())`
     the segments' shortest-first walks run together (shortest level of any
     segment first, a term's doc count summed over segments) and are cut at
     N once; the first scorer computes it, every segment uses it. Pinned
     terms (the exact word typed and its quote-free twin) always keep a slot
     when the automaton matches them, taken from the worst kept slot.
   - *Why:* the result must not depend on how the index is split (an
     update adds segments); FtsLib had one term index. On one segment it is
     the per-segment walk and costs nothing. The typed word must never be
     cut by its own expansion (issue #5).

5. **SpanQuery: per-pair word distance over any walk.**
   (`query/span_query.rs`, new)
   - *What:* "word A, then within N words word B, then within M words word
     C". Each gap has its own limit, gaps count words between, order is
     optional, and one word never fills two slots. Each slot can be any term
     source: an exact word, fuzzy, regex, or any of our Hebrew walks.
   - *Why:* upstream's PhraseQuery has one shared slop budget and plain
     terms only (RegexPhraseQuery: regex only). Otzaria's custom per-pair
     spacing over expanded words needs both.
   - *Window mode* (`set_window(Some(max_span))`): every slot at its own
     position, in any order, all inside one window of `max_span` positions
     (Otzaria's any-order proximity). One sweep over the slots' merged
     positions; a bipartite slot/position matching runs only when one
     position can fill several slots (a repeated word, overlapping walks).
   - The ordered check is a linear forward sweep per slot (every position
     where a valid chain can end), no backtracking.

6. **Indexing in the OS background mode, live memory budget** (issue #19;
   `indexer/thread_priority.rs`, `indexer/index_writer.rs`,
   `indexer/segment_updater.rs`). `IndexWriterOptions::low_priority_threads`
   puts the indexing workers and the policy's merges in the OS background
   mode, set on each thread itself: Windows `THREAD_MODE_BACKGROUND_BEGIN`
   (CPU + I/O + memory priority, fallback `THREAD_PRIORITY_LOWEST`;
   [doc](https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-setthreadpriority)),
   Linux `SCHED_IDLE` (`SCHED_BATCH` without `RLIMIT_NICE` headroom) + `IOPRIO_CLASS_IDLE`
   ([sched](https://man7.org/linux/man-pages/man7/sched.7.html),
   [ioprio_set](https://man7.org/linux/man-pages/man2/ioprio_set.2.html)),
   Android nice 10 = `THREAD_PRIORITY_BACKGROUND`
   ([doc](https://developer.android.com/reference/android/os/Process#THREAD_PRIORITY_BACKGROUND))
   + idle I/O, Apple `QOS_CLASS_BACKGROUND`
   ([doc](https://developer.apple.com/library/archive/documentation/Performance/Conceptual/EnergyGuide-iOS/PrioritizeWorkWithQoS.html)).
   Nesting-safe per thread (depth counter, as FtsLib's
   `BackgroundPriorityScope`), with `NormalPrioritySection` to leave it
   around sections foreground work waits for: a worker's commit-driven
   flush and an explicit `IndexWriter::merge` run at normal priority by
   themselves. `IndexWriter::memory_budget()` is a live handle each worker
   reads once per batch: lowering it flushes the segments above it (the
   buffer is released, as Elasticsearch's IndexingMemoryController forcing a
   flush). Public, for any thread of the application.
7. **Reordering merge** (`indexer/reorder.rs`, `merger.rs`, the postings / store /
   fast-field merge plugins; issue #29 follow-up).
   - *What:* `IndexWriter::merge_reordered(segment_ids, plan, control)` merges segments
     into one segment whose docs are in the order a caller's plan gives (any permutation
     of the alive docs), and can replace the values of `u64` fast fields on the way (the
     sort field renumbered, so an index with `sort_by_field` stays sorted; a plan against
     the sort field is refused). `ReorderControl` cancels it (the merge then fails and
     the index keeps its segments) and reports progress (plan, postings terms, store, fast
     fields). Background priority when the writer has `low_priority_threads`.
   - *Why:* a catalog order change (books moving in the tree) re-sorts the whole index
     without re-indexing: one merge instead of a full build (see "Re-sort into a new
     catalog order" below).
   - *How:* a new mapping type, `Reordered`: upstream's sorted merge assumes the new ids
     of one segment increase with its old ones (a k-way merge of postings, a sequential
     store read). For `Reordered` the postings of each term are buffered (docs with their
     positions), sorted by new doc id and written; the store reads each doc by address;
     the fast fields are written column by column in the new order with the replaced
     values (numeric and bool fast fields). Other merges are unchanged.

Also small: `get_match_term_infos` honours the cap and the sink, and
`RegexPhraseWeight` is reachable crate-internally so SpanQuery can reuse its
positioned union. `PhraseScorer` is public (its constructor already was), so
the relevance order scores a phrase with tantivy's own BM25 over a union of
the matched terms per position. `FuzzyTermQuery`'s Debug text is unchanged (the new fields
are not printed), so upstream's query-parser tests stay as they are.
`MmapDirectory`'s atomic write (`meta.json` on commit) replaces the file with
`std::fs::rename` (POSIX semantics on Windows, so a reader holding it open with
delete sharing does not block), and on Windows retries access-denied / sharing
errors for about 500 ms before a clear error; a failed commit keeps the pending
changes, so the host can retry it (issue #7 A12, as Otzaria's index directory).

### hebrew-fts
- **Tokenizer.** A port of FtsLib's `HtmlWordScanner`:
  - HTML-aware: inline tags are transparent; block tags, `sup` / `sub`, entities and maqaf
    split words, except a `<sup>` holding a vocalized Hebrew letter mid-word
    (content only Hebrew letters and marks, at least one mark, a Hebrew
    letter or mark right before `<sup` and right after `</sup>`), which is
    transparent (FtsLib `fts10`, ours `tantivy12`; issue #2 H58). The line
    regex view and the cross-line edge view keep Otzaria's rule (every sup
    vanishes); edges are mapped back onto the scanner's words. An unterminated `<` (no `>` after it in the line) is no
    tag: a plain separator, and the rest of the line is indexed (Otzaria
    keeps it verbatim; FtsLib `fts9`, ours `tantivy10`).
  - **Word rule = Otzaria's tokenizer** (`hebrew_tokenizer.rs`; FtsLib
    follows it since index `fts8`): a word starts at a letter or digit of
    any script (Hebrew, ASCII, and every other Unicode alphanumeric char:
    no Hebrew/Latin split, `abc` + Hebrew is one word) and runs over
    letters, digits and marks, any length, one letter included. Terms are
    lowercased in every script (Rust `to_lowercase`).
  - Transparent chars join a word and vanish from it: `.` `[` `]` `*` `~`
    `^` `$` `\` `+` backtick and the zero-width / bidi controls
    (U+200B..U+200F, U+202A..U+202E, U+2066..U+2069, U+FEFF): pe.bet.yod is
    one word.
  - Quotes: between letters one geresh is kept as `'`, two gereshes or one
    gershayim as `"`; another run of quotes splits. After a word a geresh
    is kept (`'`, inside the word's span), a gershayim is not. Typographic
    quotes (U+2018/2019, U+201C/201D) count as geresh / gershayim; Hebrew
    presentation forms (U+FB1D..U+FB4F) fold to their letters.
  - Paired readings `(X) [Y]` (ketiv / qere): Y takes X's position, in the
    index and in the locator's distances.
  - Every word takes a position; a word whose term (before lowercasing,
    quotes included) has 128 UTF-8 bytes or more is not a term but keeps
    its position (Otzaria's `RemoveLongFilter`, which keeps `len < 128`).
    Index, query parser and snippet locator count the same words, so index
    positions = the locator's positions and FtsLib's distance = Otzaria's.
  - `scanner_reference.rs` runs Otzaria's own functions (`word_rule.rs`,
    verbatim) on the visible text; a property test keeps the single-pass
    scanner equal to it.
  - Two variants: `hebrew` strips nikud and te'amim, `hebrew_marked` keeps
    them (and the combining accents U+0300..U+036F) in canonical order:
    the marks of one letter are sorted by their Unicode combining class
    (stable), so a text or a query typing dagesh before or after the vowel
    gives one term. U+034F COMBINING GRAPHEME JOINER (a text puts it
    between two marks to keep their order) is dropped from the term and the
    typed word before ordering, so it never decides a match; it stays inside
    the word's span, so highlight offsets count it (ours `tantivy12`;
    issue #2 H02; FtsLib's mark-free term drops it too). A kept mark counts in no length: the 128-byte limit
    measures the mark-free bytes, so both variants index the same words.
- **Walks:**
  - **Grammar** (`%word`, `word%`): 34 prefixes and 32 suffixes.
  - **Otzaria options:** grammatical/Aramaic prefixes and suffixes, ktiv
    male/haser, part-of-word, advanced prefixes/suffixes, ignore gershayim,
    typos (שגיאות כתיב), match nikud, match te'amim. Ported from Otzaria's
    code (`hebrew_query.rs`, `quote_expansion.rs`), all inside the walk:
    - **Quote-free terms** (always, unless `exact_quotes`): an index term also
      matches by its quote-free form, so `רמבם` finds `רמב"ם` (Otzaria's
      quote-free twin token, done at query time; the index keeps no twin).
      `רמב"ם` typed still needs the quote unless `ignore_quotes`.
    - **כתיב מלא/חסר**: Otzaria's י/ו rules: internal letters only, one
      deletion per run, no adjacent insertions, 3-letter words only take ו
      after the first letter, no י/ו dropped before a final ה (מצות ↔ מצוות,
      also מצת as in Otzaria; אותו ↛ אות; שבת ↛ שבות). Cap 32 (16 with
      grammatical options).
    - **סיומות דקדוקיות**: Otzaria's suffix table (not FtsLib's `%` table),
      past-tense endings for 3–4 letter roots, final letter in its base form
      (מלך → מלכים), final ה → ת/ות (תורה → תורתו).
    - **Root floor**: grammatical prefixes and suffixes need a root of 3
      letters (אב ↛ אבות, ית ↛ בית); abbreviations are exempt (ר' → ר'ים).
    - **קידומות דקדוקיות**: Otzaria's `prefix_group`, three optional slots:
      one of ו מ דא כש כ ב ש ל ה ד, then one of כש כ ב ש ל ה ד, then ה
      (up to 5 letters: דאכשה־); Aramaic prefixes add א to the first slot.
      Composes with the advanced prefix window (union, as Otzaria's lead).
    - **Any-letter windows** (advanced prefixes / suffixes, part of word):
      Otzaria's `AFFIX_LETTER`: each unit is a Hebrew letter U+05D0..U+05EA,
      a Yiddish ligature U+05F0..U+05F2 or a geresh / gershayim (`'` `"`),
      never a Latin letter, a digit or another script. The window before
      the root also takes Otzaria's `NOTE_DIGITS`: any ASCII digits ahead of
      its letters (a glued footnote number), free of the letter budget; the
      window after the root takes none (issue #2 H05). In vocalized
      matching (typed, required marks; every other mark free) a free mark
      follows a letter: never before, between or right after those digits,
      nor before the word's first letter (Otzaria's vocalized
      `[0-9]*(?:letter marks*){min,max}`).
    - **שגיאות כתיב**, two behaviours, chosen by `typo_mode` (letters
      counted without quotes/marks; ≤2 letters never take a typo):
      - `otzaria` (default, exactly Otzaria): a lone word of ≥4 letters
        whose only option is typo gets edit distance 1 (substitution,
        deletion, insertion, adjacent transposition: Otzaria's
        Levenshtein-1 scan). Otherwise (another option, a phrase word, or
        3 letters) Otzaria's sampled list `generate_typo_variations`:
        confusions (א/ע/ה, ח/ה, ו/ב, כ/ק, ט/ת, ס/ש, צ/ז) and adjacent
        transpositions, then deletions, then insertions of its 13 letters;
        3 letters only confusions; cap 128 roots (64 in a phrase), each
        through the other options.
      - `full_edit1`: edit distance 1 for every ≥4-letter word in every
        combination; 3 letters only the confusions.
      The edit distance is edit states of the walk automaton's core phase;
      the sampled list is core forms of the same walk's trie. Either way one
      walk per word, shortest-first and capped, with the matched-terms sink.
      Not ported: Otzaria's phrase char budget (`MAX_PATTERN_CHARS`), which
      only trims regex text it compiles.
    - **Plain fast path**: a word whose options only add core forms (none,
      ktiv, Aramaic endings, the typo list) walks the core trie alone, with
      no per-byte option checks.
  - **Length limits are set by the caller** (Otzaria), as plain ints; unset
    means the default:

    | Option | Default |
    |---|---|
    | `PrefixMaxLetters` | 3 (5 for a word of ≤3 letters) |
    | `SuffixMaxLetters` | 5 (7 for a short word) |
    | `PartOfWordMaxLetters` | 3 per side (2 for 4+-letter words) |
    | `GrammarPrefixMaxLetters` | none (Otzaria's full set, ≤5 letters) |
  - **Wildcard** (`*`, `?`) with FtsLib's limits:
    - at least 2 real letters, at most 4 `?`
    - `*` may cover 3 characters before the anchor, 4 after it, or 7 in total
      when both ends are wildcards
- **Query parser.** FtsLib's operators over Otzaria's word rule
  (`query_parser.rs`; FtsLib's `QueryParser` is the same): a paired
  reading `(X) [Y]` keeps Y (Otzaria's `keep_second_of_paired_readings`);
  separators split words, `a | b` is one OR slot, `*` `?` wildcard,
  `word~N` fuzzy (N clamped to 2), `%word` / `word%` FtsLib's grammar
  tables. The words of a typed word are cut by the text's rule (`*` and `?`
  count as word chars), so a query word is normalized exactly as the index
  term. A literal of 128 bytes or more is `QueryTerm::Unindexed`: no term,
  one position (see the proximity item). One addition: on an index that
  keeps marks, a plain word keeps its typed nikud / te'amim (vocalized
  search). It only tokenizes and numbers the words; the engine groups them
  and applies the options.
- **User regex queries `/.../`** (`user_regex.rs`, Otzaria's
  `user_regex.rs` `parse` / `fold_pattern` / `prepare_term_part` and its
  `term_automaton`; opt-in: `user_regex` in `SearchOptions`, C#
  `UserRegex`, Dart `userRegex`, off by default so FtsLib mode is
  unchanged; `user_regex_kind` / C ABI `se_user_regex_kind` / C#
  `SeforimIndex.UserRegexKind` / Dart `SeforimIndex.userRegexKind` tell a
  host what a query is, as Otzaria's `parse_user_regex`):
  - Detected when the trimmed query is longer than 2 chars, starts and ends
    with `/` and has no slash next to a space between the outer slashes
    (`/a/ /b/` is two words). The whole query is the regex, on its own:
    word options, distance, negative query and phrase alternatives do not
    apply (as Otzaria). An invalid pattern (syntax, over 300 chars, only
    anchors/spaces, too complex) is searched as plain text with every
    option, as Otzaria's app does (the Dart demo asks the kind first).
  - Folded like the index text (marks dropped, maqaf/paseq a space, quotes
    ASCII, presentation forms decomposed), parsed case-insensitively in the
    Rust `regex` dialect, edge anchors and spaces dropped, split on
    whitespace separators (a space run, `\s`, `\s+`).
  - Each word compiles like Otzaria's: classes narrowed to its term
    alphabet (quotes, digits, a-z, Hebrew letters), lazy made greedy,
    bounded repeats above 7 unbounded (a second, aggressive pass when the
    cost is too high), an anchored dense DFA (`regex-automata`), and a
    verifier (`\A(?:narrowed)\z`) when relaxing let in more terms.
    The DFA is walked over the term dictionary (`GroupSpec::Regex`,
    `HebrewRegexQuery`), whole-term, quote transparent, verified at each
    match, with the matched-terms sink. One word (Otzaria's `Word`): that
    term set. Several words (`Sequence`): each a walk, adjacent and in
    order (a span query with positions; the locator checks it on the text
    without them; both give the same lines and counts).
  - Deviations, named: the cap is ours (`max_expanded_terms`, default 2000, shortest first,
    like every walk) where Otzaria takes up to 10000 terms (a sequence
    8192 per word) in dictionary order under postings budgets, and reports
    `truncated`; ours reports the cap through the search's truncation flag. Narrowing to Otzaria's term
    alphabet means a letter outside it, literal or class (an accented
    Latin letter, Cyrillic, Greek), never matches (as Otzaria). A byte
    class (`(?-u:...)`) is invalid, as Otzaria. The low-level request's
    `{"kind":"regex","pattern":...}` takes a pattern as is (no folding, no
    case folding, no 300-char limit; still whole-term and size-bounded). Otzaria's sequence may also continue onto the next line: that
    part is deferred with cross-line matching. An index that keeps marks
    reads the term marks as zero width (`MarkFree`), so the folded pattern
    matches what it matches on the mark-stripped index.
  - Otzaria's `Line` kind (`CrossSpace`: an explicit space class such as
    `[ ,]`, inner anchors or `\b`, a wildcard-only word in a sequence) is
    matched on each line's TEXT, ported as Otzaria does it
    (`hebrew-fts/src/line_regex.rs`: `LinePlan`, `required_clauses`,
    `LineView`; `seforim-engine/src/line_search.rs`: `candidate_query`,
    the verify / scan loop). Kind `"line"` from `user_regex_kind`.
    - Plan: the regex with a word boundary implied on each edge that can
      be a word char, plus a prefilter without word-boundary assertions;
      run on Otzaria's line view (markup removed, nikud / te'amim dropped,
      maqaf / paseq / sof pasuq / nun hafukha / `|` a space, spaces
      collapsed, quotes ASCII, presentation forms decomposed, lowercase),
      with a map from the view back to UTF-16 offsets. Word breaks follow
      the index's scanner (issue #15): a tag breaks where the scanner breaks
      (block tags, `<sup>` / `<sub>` unless a vocalized letter mid-word),
      other tags vanish, and every entity or bare `&` is a space (Otzaria's
      view drops non-space entities and keeps a bare `&`; that would glue
      words the index keeps apart, so a pattern cannot match a literal `&`).
    - Candidates from the index (bare-minimum rule: no text in the index):
      the pattern's required fixed words, ANDed rarest first, inside the
      book filter, a guaranteed SUPERSET of the lines the text check
      matches (issue #15, with Otzaria PR 66's fix). Each literal run is
      cut into terms by the index's own scanner (quotes, `''`, transparent
      `.`, case and marks as indexed); a term is walked over the dictionary
      quote transparent and anchored at a term edge only where the pattern
      names a separator right there (`frag`, `frag.*`, `.*frag`,
      `.*frag.*`): a pattern edge or a wildcard leaves it open, since the
      line may glue letters there (`x.ab`, `x"ab`). Lines with a word too
      long to be a term (128 bytes or more, line flag `LINE_FLAG_LONG_WORD`,
      bit 3 = value 8) are always candidates. Prefix-anchored clauses run first; once
      one has 10,000 postings or fewer, clauses needing a
      full-dictionary walk are skipped (more candidates, never fewer). A
      clause over 3,000,000 postings or a fragment over 50,000 terms is
      dropped (Otzaria's budgets). The candidates are the search's hits,
      catalog order. The oracle test (`candidates_are_a_superset_of_the_text_check`)
      checks candidates + verify == full scan on fixtures and random sets.
    - Full scan when no clause is selective (or on a keep-marks index,
      whose vocalized terms the folded fragments cannot name): no hits;
      the host streams every line in scope, catalog order, with its
      placement.
    - The host feeds texts in batches; the engine matches them across all
      cores and returns each line's matches as UTF-16 `(start, end)`
      ranges. At most 100,000 matching lines (Otzaria's `MAX_LINE_HITS`),
      then the search is truncated (the truncation flag). Every batch
      checks the cancel token. Rust `SearchResult::line_regex` /
      `line_match_batch` / `line_scan_batch` / `line_hits`; C ABI
      `se_search_line_status` (JSON) / `se_search_line_match_batch` /
      `se_search_line_scan_batch` / `se_search_copy_line_hits`; C#
      `SearchSession.GetLineRegexStatus` / `LineMatchBatch` /
      `LineScanBatch` / `GetLineHits` (demo `TantivySeforimIndex.RunLineRegex`,
      used by `count --regex`); Dart `lineRegex()` / `lineMatchBatch` /
      `lineScanBatch` / `lineHits` (the Otzaria demo prints the matches).
    - A host that only locates snippets keeps its usual loop on the
      candidates: `locate` returns the whole line as the window (never
      short) with every match marked, and `count_batch` counts through
      the same cap. A line pattern that cannot compile is searched as
      plain text (kind `"invalid"`), as Otzaria's app does.
    - Deviations, named: Otzaria's per-search result cache, book-wise
      reading strategy (5,000-candidate threshold) and snippet painting
      are host concerns here; the 20M-keys-per-segment scan budget is not
      ported (one merged segment, about 1.8M terms).
  - Measured (full index, C# demo `count --regex`, positions / no
    positions): a one-word pattern with `?` 102,774 lines in 209 / 284 ms;
    a broad `.*` word (capped) 707,649 lines in 285 / 216 ms; a two-word
    sequence 15,228 lines in 122 ms / 0.05-0.12 min (20k texts fetched from SQLite and validated; run to run); a leading
    class `[...]+` suffix 44,780 lines in 0.04 min; `\w{2,5}` (capped)
    6,265,804 lines in 0.07 / 0.05 min; a verified `\w{1,30}` + suffix
    44,780 lines in 0.08 / 0.04 min.
- **Snippet locator.** A step-for-step port of FtsLib's
  `PreparedQueryGroups`, `SnippetWindowLocator` and `SnippetDescriptor`.
  Offsets are UTF-16 (.NET/JS string indices).

### seforim-engine / engine-ffi
- **One word model** (no option): FtsLib's, which is now Otzaria's (see
  the tokenizer). A one-letter word or digit run is a query slot of its own,
  matched at its position (`a x b` checks `x`), and every word counts in
  distances, on both index flavours. The `word_model` option of the
  previous version (ftslib 2..29 letters / otzaria) is gone: nothing
  differs any more once FtsLib counts every word.
- **Book updates are atomic by host policy** (issue #32): one book update = delete
  its lines + add its new lines + ONE commit. Never commit, `merge_all` or
  `se_writer_merge_and_close` (C# `MergeAndClose` / Dart `mergeAndClose`) in between. A crash before
  that commit leaves the old book whole (a tantivy commit is atomic). Use the commit
  payload as the host's pending/done record (e.g. the last finished book) to resume.
  The engine has no book-level API for it. Tested in
  `seforim-engine/tests/book_update_atomicity.rs`.
- **Re-sort into a new catalog order** (optional; a build never needs it): when books
  move in the catalog tree, `LineWriter::resort_and_close(book_order, cancel, progress)`
  (C ABI `se_writer_resort_and_close` with `se_resort_progress_new` / `_get` / `_free`;
  C# `SeforimIndexWriter.ResortAndClose` / `ResortAndCloseAsync`, Dart
  `IndexWriter.resortAndClose` + `ResortProgressCell`; demo `resort
  [--simulate-move-tenth]`) brings an existing index to the new order without
  re-indexing. The host passes every indexed book id in the new order (books without
  lines may appear; indexed books it does not list go last, in their old order; a book
  listed twice is an error). Each book
  keeps its lines in their order and spacing and the books are packed from position 0,
  so when the old positions were the host's dense catalog numbering the new ones are
  what a fresh build in the new order gives. ONE reordering merge (tantivy item 7)
  writes every line in the new order into one segment with `position` rewritten: the
  index stays catalog-ordered (no sort at search time), the publish is atomic (searches
  see the old order until the new segment is complete), the order stamp changes (a
  resume from a page of the old order fails with `StaleResume`, issue #12), the old
  segment's files and sidecar are removed (issue #29) and the FM sidecar is rebuilt.
  Cancelling before the publish leaves the old index (`Cancelled`); progress stages:
  plan, postings (terms), store, per-line fields, publish, FM sidecar, done. Background
  priority when the writer has it (issue #19). The commit payload is kept: a host that
  records its catalog in the payload commits the new record afterwards. Peak disk: the
  old and the new segment together (about twice the index). Full DB (7.9M lines, index
  with positions 1.76 GB, SSD): 2.56 min (plan 263 ms, postings 1.9 min, per-line fields + store 0.3 min, FM sidecar
  0.1 min; one tenth of the books moved), peak working set 2,582 MB, peak disk 3,434 MB (old +
  new segment); a plain one-segment merge on the same SSD takes 1.23 min with its sidecar. Tests: `seforim-engine/src/resort.rs`,
  tantivy `indexer/reorder.rs`, Dart `resortAndClose`.
- **Index:** create or open; positions optional; add/delete/commit lines
  (`line_id`, `position`, `book_id`, text indexed but not stored; see the
  bare-minimum rule above) with a host payload (resume watermark,
  provenance stamp) committed atomically with the documents. C ABI:
  `se_index_create` / `_open` / `_exists` / `_info` / `_close`,
  `se_writer_open` / `se_writer_open_ex` (flags: `SE_WRITER_AUTO`, the
  SDKs' default, see "Writer resources" below; `SE_WRITER_LOW_PRIORITY`: fixed
  resources with the indexing threads and background merges in the OS
  background mode; Dart `openWriter()` / `openWriter(heapBytes:, threads:,
  lowPriority:)`, C# `OpenWriter()` / `OpenWriter(heapBytes, threads,
  lowPriority)`; a fixed writer's default heap is 50 MB per thread, so the
  default threads, cores up to 4, are always granted), `se_writer_memory_budget`
  (current budget per thread), `se_thread_background_begin` / `_end` (the
  calling host thread in the OS background mode; C# `BackgroundPriority.Enter()`,
  Dart `runInBackgroundPriority`), `se_writer_add` / `se_writer_add_line` (per-line flags,
  see cross-line below), `se_writer_add_lines` (a batch in one call:
  48-byte `SeLineRecord`s of `se_writer_add_line`'s fields + text length,
  the UTF-8 texts concatenated in one buffer; Dart `IndexWriter.addAll`),
  `se_writer_delete`, `se_writer_commit` /
  `_rollback` / `_merge_and_close`, `se_index_build_fm_sidecar` /
  `se_index_has_fm_sidecar`, `se_index_num_lines` (a cheap count, no JSON; C#
  `SeforimIndex.NumLines`, Dart `numLines`), `se_index_reload`. One shared
  reader per open index (issue #22): every search, count and sidecar check
  takes its searcher from it; the index's own writers reload it after every
  commit, rollback and merge (no file watcher), and a running search keeps
  the snapshot it started with. A commit is reported by its durability: if
  the reload after it fails, the commit still succeeds, searches keep the
  previous snapshot and the next search or count retries the reload (Rust
  `LineIndex::reload_pending`; `se_index_reload` forces it and reports the
  error). After every commit, rollback and merge, and when a writer opens,
  the writer deletes the files no snapshot uses any more (issue #29: merged-away
  segments and their `.fmi` sidecars stayed on disk for good, doubling the
  index), plus leftover sidecar build temps (same rule as on open). A search still holding an old snapshot keeps reading it; a file
  that could not be deleted then is retried at the next write or writer
  open. When another process or another handle on
  the same directory writes the index, the host calls `se_index_reload`
  (C# `Reload()`, Dart `reload()`) or reopens. Options are strict both ways: create's
  options JSON may omit a key (its default) but malformed JSON, a
  non-object, an unknown key or a wrong type is an error; open reads the
  recorded `engine_options.json` (both keys required, no unknown key) and
  checks the schema agrees with it (text tokenizer, positions record
  option, id fields), so a missing, malformed or mismatched recipe fails
  the open instead of opening with defaults.
- **Hits:** `PlacedHit` = position, line_id, book_id; the C ABI's
  `se_search_copy_hits` writes 3 u64 per hit in that order.
- **Filtered search by book:** `books` in the request (C# `Books`, Dart
  `books`, demo `--book TEXT`): only lines whose `book_id` is in the set
  come back (an empty set = no hits). Checked per doc on the `book_id` fast
  column during collection, so it covers the main query, phrase
  alternatives and exclusion alike.
- **Exact totals and per-book counts** (Otzaria's `count` /
  `count_by_book`): the number of lines that pass EVERY filter (distance,
  order, proximity, phrase alternatives, negative query), in all and per
  `book_id`. Rust `SearchResult::counts` / `count_batch`, C ABI
  `se_search_count_needs_text` / `se_search_count_batch` /
  `se_search_counts` (JSON `{total, books:[[book_id,lines]], validated,
  exact}`), C# `SearchSession.CountNeedsText` / `CountBatch` / `GetCounts`
  (demo: `TantivySeforimIndex.CountByBook`, command `count`), Dart
  `countNeedsText` / `countBatch` / `counts()` (demo `--count`).
  - When no check is left for the text (no FtsLib distance/order, the
    proximity or phrases already ran as span queries on positions, no
    text-level negative; or a one-word query), the hit list IS the answer: counted from the
    `book_id` fast column, no text read.
  - On an index with positions every hit is decided from its positions
    during the search (issue #21, see "Hits decided from positions"): the
    hit list holds only passing lines, `count_needs_text` is false and the
    counts are exact once the search is complete, with no text read.
    Cross-line pairs too: the cross-line pass decides them on the two
    lines' joined positions.
  - Otherwise (an index without positions) the host
    feeds the hits' texts in hit order (any batch size); the engine
    validates each batch across all cores with exactly the snippet
    locator's test (no snippet, no term table) and adds the passing lines.
    `exact` turns true when every hit is counted. This is FtsLib's path.
- **Cross-line matching** (Otzaria's `continues_to_next_line`,
  `cross_line.rs`; `cross_line` in `SearchOptions` / `SearchRequest`, C#
  `CrossLine`, Dart `crossLine`, Otzaria demo `--cross-line`; off by
  default, so FtsLib mode is unchanged). A match may start at the end of
  line L and end at the start of line L+1 of the same book (L+1 = the next
  catalog position), at most one break, for every way of matching: the
  main query's rule (Otzaria's ordered or any-order proximity, per-pair
  gaps in order, FtsLib's distance / order, words anywhere, partial
  matching) and each phrase alternative (so user-regex sequences too).
  `seforim-engine/src/cross_line.rs`:
  - The two lines are one virtual line of their content words (Otzaria's
    `content_range`, `hebrew-fts/src/line_edges.rs`): L's positions up to
    its last content word, then L+1's from its first content word, right
    after it; a trailing enumerator of L and a leading one of L+1 (`(x)`,
    `[x]`, `{x}`, `(49):`; 1-5 letters, digits, quotes or `*`; a `(X) [Y]`
    paired reading is content) take part neither in the gaps nor in the
    matching. The rule must hold there with a matched word on each line. A line with the no-cross flag (headings; the C# and Dart
    builds set it for a line starting with `<h`, Otzaria's rule) takes part
    in no cross-line match, into or out of it; a line with no content word
    (empty, or only an enumerator) neither.
  - Candidates by shift-and-AND over per-slot line bitmaps (catalog
    positions), built from the terms the walks already matched (the
    sinks: postings only, no second walk): in order, for each split s the
    lines holding slots 0..s AND the lines holding slots s.. shifted down
    one; in any order, each slot on L or L+1 with a slot on each line (a
    per-line count for partial matching). Then L+1 alive in the same book,
    neither line flagged, the book filter, the negative query and the
    negative section scope on L (Otzaria applies its filters to the
    attributed line).
  - In-line wins: a line found in-line is not a cross-line hit (removed
    from the candidates when the in-line hits are exact; otherwise the
    text check tries the line alone first).
  - Exact check: words anywhere and partial matching are exact from the
    bitmaps (unless an enumerator sits at the break: then the text check
    decides, its word may be the slot's only occurrence there); proximity, gaps and phrases are checked on the postings'
    positions of both lines on an index with positions (the locator's own
    window rules over synthetic tokens, `proximity_across_positions`);
    without positions, and for FtsLib's distance / order, the host's two
    texts are checked by the locator over the joined text (`tokenize_pair`
    drops the break's enumerators the same way). Both flavours give the
    same hits.
  - The host feeds the per-line data at indexing with
    `se_writer_add_line` (flags `SE_LINE_NO_CROSS` = 1 for a heading,
    `SE_LINE_HAS_SECTION` = 2 when `section_id` is set).
  - In a vocalized search only lines carrying a mark take part (both L and
    L + 1), as in-line.
  - A cross-line hit is reported on L with `next_line_id` (C ABI
    `se_search_copy_next_lines`, `u64::MAX` = none; C# / Dart
    `PlacedHit.NextLineId` / `nextLineId`). The host locates it with both
    texts (`SnippetSession::locate_pair` / `try_locate_pairs`, C ABI
    `se_search_locate_pairs`: per hit `[0]`, `[1, snippet]` in-line or
    `[2, break, snippet]` cross-line; C# `LocatePairs`, Dart
    `locatePairs`); counts take the pairs too (`count_batch_pairs`,
    `se_search_count_batch_pairs`, `CountBatchPairs`, `countBatchPairs`).
    A cross-line snippet's offsets are UTF-16 into `line + "\n" + next`
    and `break_at` (C# `BreakAt`, Dart `breakAt`) is that `\n`'s offset
    (render a line break there). Each line is tokenized on its own (no
    word, paired reading or tag state crosses the break).
  - Deviations, named: the content edges are computed by the engine at
    indexing, not supplied by the host (they must equal the index
    positions, which only the engine's scanner numbers). Relevance order: cross-line hits score 0
    (after every in-line hit; Otzaria scores them below typical in-line
    hits). Per-pair `gaps` in either order (low-level request only) are
    refused with cross-line. On an index with several segments the
    candidates come from the union of the segments' matched terms (merged
    indexes have one segment).
  - Needs an index with the per-line fields (`se_index_info` `line_data`);
    an older index returns a clear error.
- **Same-section scope** (Otzaria's `SearchScope::SameSection`,
  `section_scope.rs`; `same_section` in `SearchOptions`, C# `SameSection`,
  Dart `sameSection`, Otzaria demo `--scope section`): every word (or
  `word_match`'s count) anywhere in one section of the host's, the words
  spread over its lines. One pass: each word's walk yields its lines
  (filling its sink), each word's lines become the set of sections they
  fall in (`(book_id, section_id)`, so ids may repeat across books; a line
  without a section id is a section of its own), the sets are intersected
  (or counted per section for at-least-N / most), and the hits are the
  lines holding a word in a surviving section; one surviving word is
  enough below 2 (Otzaria's flat OR). Distance, order and proximity do not
  apply; phrase alternatives still add their own lines; `cross_line` is
  ignored. Both index flavours; no text check (`count_needs_text` false).
  Relevance: the count score (1 per slot the line holds).
  - The negative query's scope (`exclude.same_section`, C# / Dart
    `ExcludeOptions.SameSection` / `sameSection`, Otzaria demo
    `--exclude-scope section`): a section holding every negative word
    (each anywhere in it; a negative phrase alternative: its words
    anywhere in it) is dropped whole, in the collector, through the same
    section sets. This replaces the Dart demo's host-side multi-search
    exclusion. A line without a section id is its own section (the old
    host path never dropped such a hit).
  - Sections come from the host at indexing (C# demo and Dart demo:
    `line_toc.tocEntryId`, else `line.tocEntryId`; none = no section). An
    index without the field returns a clear error.
- **Resume after a position** (KitveiHakodesh 277de7a9, FtsLib's
  `afterPosition` = `rowid > position`): `after_position` in
  `SearchOptions` / `SearchRequest` (C ABI JSON `"after_position"`, C#
  `SearchOptions.AfterPosition`, Dart `afterPosition`) keeps only the lines
  at catalog positions strictly after it; None = from the start, no cost.
  - Applied while collecting: a position-sorted segment (every segment of a
    `sort_by_field` index) seeks straight to the first doc past the bound
    (binary search on the `position` column), and the collector also drops
    any position at or before it, so several segments, an update's extra
    segment and old unsorted indexes stay correct (alive bitset checked as
    always). Cross-line hits are bounded by their first line; a line regex
    bounds its candidates, and its full scan treats earlier lines as out of
    scope (`scope_lines` counts the rest). Totals and counts are the rest's,
    as FtsLib's resumed query sees only the rest.
  - Resume guard (issue #12): every search reports an `order_stamp`
    (`SearchInfo`; C# `SearchInfo.OrderStamp`, Dart `orderStamp`), a hash
    of the searched snapshot's segment ids and delete opstamps. Passed back
    as `resume_stamp` (C# `ResumeStamp`, Dart `resumeStamp`) with
    `after_position`, a changed index (rebuild, update, merge) fails with
    `Error::StaleResume` (message prefix `stale resume:`; C#
    `SeforimEngineException.IsStaleResume`, Dart `StaleResumeException`)
    instead of skipping or repeating hits: the host restarts. No stamp = no
    check (FtsLib). The C# FtsLib facade resumes by line id, re-placed on
    the current index (Core's Restarted), so it needs no stamp.
  - C# demo, as Core: `SeforimDbFtsEngine.PositionOfLine(lineId)` (book
    position ranges cached, plus the line's rank in its book:
    `FtsLineRankInBook`), `SeforimDbFtsSearcher.SearchInBatches(...,
    afterPosition)` (a resumed run seeds no snippet texts) and
    `TantivySeforimIndex.Search(..., afterPosition)`. Positions there are
    FtsLib's (from 1); the facade passes `afterPosition - 1` to the engine
    (whose positions start at 0). A line that cannot be placed (null) means
    restart from the start, with `Restarted` on the first frame.
  - C# demo stream coordinator (Core's ftsSearchStream, ported step for
    step): `SeforimDbFtsIndexCoordinator.StreamSearch(query,
    maxWordDistance, requireOrdered, contextWords, emit, clientCt,
    resumeAfterLineId)` pushes `FtsStreamChunk` frames (same fields as
    Core: Ready, Results, NewTerms, SnippetTexts, Done, Error, Restarted,
    Stage): a `validate` frame first (carrying `Restarted`), stage-only
    frames until the first results frame, then stages folded into results
    frames, then one `Done`. Each stream has its own term table, so a
    resumed stream numbers its terms from 0 again (the receiver maps
    them). No totals ride on the stream (as Core). A new search supersedes
    the previous one; a cancelled client gets no `Done` frame. `search
    --resume-after <lineId>` runs through it and prints the flag.
    Differences from Core, by design of the demo: no background index
    build (`EnsureIndexing`; ready = the index is built), no related-form
    `rewrite` stage (the demo has no related-form expander), and a
    demo-only `WordOptions` property (Otzaria's per-word options; null =
    Core's behaviour).
- **Non-blocking search** (issue #16, in our packages): Dart `SearchWorker`
  (`dart/seforim_search/lib/src/async_search.dart`): a worker isolate that opens
  its own index and serves searches one at a time, reused across searches (pool
  several for parallel ones). `searchStream(options, cancel:, pageSize:)` streams
  `HitBatch`es (request id, offset, hit count, hits), pulled page by page; the
  native session is freed on the worker when the stream ends, errors, or the
  listener cancels (which cancels its own token). `search(...)` returns an
  `AsyncSearchSession` with the sync session's calls as Futures (hits, locate
  batch / lines / pairs, count batch / pairs, counts, info, new terms, match
  mode). Cancel through `SearchCancelToken` (its native flag is set from the
  calling isolate, so a busy worker still stops); errors arrive typed
  (`SearchCancelledException`, `StaleResumeException`). Line ids only: texts stay
  in the host. C# `SeforimAsyncExtensions` (`csharp/SeforimSearch.Engine/AsyncSearch.cs`):
  `SearchAsync`, `SearchBatchesAsync` / `HitBatchesAsync` (`IAsyncEnumerable<PlacedHit[]>`
  via Microsoft.Bcl.AsyncInterfaces on netstandard2.0; the session is disposed when
  the enumeration ends), `GetHitsAsync`, `LocateBatchAsync` / `LinesAsync` /
  `PairsAsync`, `CountBatchAsync` / `PairsAsync`, `GetCountsAsync`; the
  `CancellationToken` drives the native cancel. Sync APIs unchanged; the Dart
  demo's result page runs through `SearchWorker`.
  Opening off the UI thread (issue #34): C# `SeforimIndex.OpenAsync` /
  `CreateAsync` (thread pool, a token cancelled before the open gives a cancelled
  task); in Dart `SearchWorker` already opens its index inside the worker isolate.
  No FM preload: the FM sidecar is memory-mapped (`fm_sidecar.rs`, as FtsLib's
  `FmIndex` uses a `MemoryMappedFile`), so opening keeps only the mapping and the
  OS pages it in on use; a preload would gain almost nothing. Cold start / first
  frame are not timed yet (needs a full-DB run, on request).
- **Cancellation and streaming** (Otzaria's cancel token / streamed pages):
  a `CancelToken` (Rust `LineIndex::search_cancellable` /
  `search_options_cancellable`; C ABI `se_cancel_token_new` / `_cancel` /
  `_free`, `se_search_cancellable`, `se_last_error_cancelled`; C#
  `SeforimIndex.Search(SearchOptions, CancellationToken)`, which throws
  `OperationCanceledException`; Dart `search(options, cancel:
  SearchCancelToken())`, `SearchCancelledException`, `cancelAt(address)`
  from another isolate).
  - Checked before the walks, per segment, every 4096 collected docs (the
    collection then walks the scorer itself), and per chunk of every
    `try_locate_many` / `count_batch` of the session (`locate_many` gives
    all-None once cancelled). The 4096 counts matched docs, deleted ones
    included. A search without a token runs the old path (no check, no
    cost); with one, the same speed (234k-hit count: 99-123 ms with a
    token, 117-310 ms without: run-to-run noise). The C# FtsLib facade
    binds its ct to `CountByBook` only: its search sessions outlive the
    enumeration, and FtsLib's locate calls never throw.
  - Streaming is pull-based, by design of the bare-minimum rule (texts come
    from the host): the host pages the hits (`copy_hits` / `GetHits`) and
    locates or counts them batch by batch, each batch cancellable. The
    walks themselves run inside tantivy's scorer creation and are not
    interrupted (they are capped).
  - On an index with positions the search itself streams (issue #21): the
    hits are decided on a producer thread in chunks (1024 candidates first,
    doubling to 65536) and sent in the search's order as each chunk is done;
    a catalog-order search of the position-sorted index reads them straight
    from the query's scorer, any other order collects and sorts first, then
    streams the decision. Rust `LineIndex::search_streaming` /
    `search_options_streaming` return after the first batch,
    `SearchResult::pull(min_hits, timeout)` takes more in,
    `is_complete()`; C ABI `se_search_stream` / `se_search_stream_pull`
    (ABI 4); C# `SeforimIndex.SearchStreaming`, `SearchSession.Pull` /
    `IsComplete`, `SearchStreamingAsync` / `PullAsync` (and
    `SearchBatchesAsync` / `AllHits` wait for each page); Dart
    `searchStreaming`, `SearchSession.pull` / `isComplete`, worker
    `searchStreaming` / `AsyncSearchSession.pull`, `searchStream` pages as
    they arrive (`HitBatch.complete`). Every session call (copies, locates,
    counts) works on the hits taken in so far; the counts are exact once
    complete; the cancel token stops the producer at its next chunk (the
    next pull fails as cancelled). The blocking `search` drains the stream,
    so its results are complete as before. A cross-line search streams too:
    each chunk's cross-line pairs (lines `L` from the previous chunk's end to
    the chunk's last in-line hit; `L + 1` may be the next chunk's line, read
    from the index) are decided with the chunk and merged into it in catalog
    order. An index without positions is complete at once (not streamed).
- **Search:** every matching line in catalog order (or by relevance /
  generation, see the result order item), plus a snippet session.
  - The session locates snippets for a whole batch across all cores.
  - Batch parallelism (issue #24): a batch of texts (locate, count, line
    regex feed / full scan) runs on the caller's thread while it is small,
    and on a persistent normal-priority search pool (rayon, one thread per
    core, built once on first use; `pools::search_pool`) from a measured
    cut-off: 32 KiB of text for locate / count, 8 KiB for line regex (it costs
    more per byte). Positions reads use the same pool. The old per-call
    thread spawn cost about 1 ms a batch (16 short lines: 0.04 ms, 32: 1.1
    ms); see `examples/search_pool_bench.rs` and `pools.rs` for the numbers.
  - It applies the word-distance and order filters. A short window's
    neighbour context is the host's job, presentation only (no neighbour or
    DB logic in the engine, and no second locate).
  - It keeps the term table.
- **Search API: one `SearchOptions` object** (Rust `LineIndex::search_options`,
  C ABI `se_search` with a JSON that has `"query"`, C# `SeforimIndex.Search(SearchOptions)`,
  Dart `SeforimIndex.search(SearchOptions)`; same field names in each
  language's casing). Callers never build walks or slot kinds: the engine
  parses the query and builds one walk per word with all its options.
  - `query`: FtsLib syntax (`*` `?` `%` `~N` `|` stay in the string).
    Quoted phrases (issue #2 H57, Otzaria's `smartSearchQuotedPhrases`
    rule, `hebrew_fts::quoted_phrases`): `"`, gershayim and the typographic
    double quotes open / close a phrase, unless the quote stands between two
    Hebrew letters (marks skipped: an acronym's gershayim, which quotes
    nothing). A phrase's words are literal: their operators, options and
    alternatives are dropped, only the quote / nikud / te'amim rules stay.
    One phrase holding every word is Otzaria's literal phrase search: its
    words adjacent and in order (the distance, order, partial-matching and
    same-section settings give way), and the match may cross into the next
    line (`cross_line` forced on). A phrase among other words is required in
    the hit's line besides them (`SearchRequest::required_phrases`: a span
    query with positions, an AND checked on the text without; cross-line off,
    as Otzaria's smart search is per line). FtsLib follows the same rule, per
    line only (it has no cross-line matching). Only with `lexical_fuzzy`
    (Otzaria's fuzzy mode with its dictionary) a quoted word keeps its own
    slot (fuzzy, dictionary forms) and the phrase allows at most one word
    between neighbours, in order (Otzaria's lexical fuzzy phrase, slop 1;
    `SearchRequest::required_phrase_gaps`, per pair; a whole quoted query
    is per line then). A fuzzy suffix before a closing quote is still the
    word's suffix (`"a~1 b~1" c~1`), in both parsers: outside lexical fuzzy
    mode (and in FtsLib) the phrase words stay literal, with no stray digit.
  - Word options, for every word: `grammar_prefixes`, `grammar_suffixes`,
    `aramaic_prefixes`, `aramaic_suffixes`, `ktiv_male_haser`,
    `part_of_word`, `advanced_prefixes`, `advanced_suffixes`,
    `ignore_quotes`, `exact_quotes`, `spelling_errors`, `match_nikud`,
    `match_teamim`, `typo_mode`, `ktiv_rules` (`"otzaria"`, the default, or
    `"ftslib"`: which rules `ktiv_male_haser` uses), `prefix_max_letters`,
    `suffix_max_letters`, `part_of_word_max_letters`,
    `grammar_prefix_max_letters`. Wildcard, fuzzy and `%` words take only
    `ignore_quotes` (as FtsLib's parser), and `ktiv_male_haser` for a
    star-only wildcard core or a `%` word (their spelling variants ORed
    in, as FtsLib's ketiv, by the word's `ktiv_rules`).
  - `words`: per-word overrides by word index. **Word index rule:** the
    place, from 0, of the word as the user typed it: the query cut on
    space / tab / CR / LF and `|` (`|` is never a word, spaced or not; a
    word the engine drops, e.g. `,`, keeps its place). When
    the word rule splits a typed word (maqaf, punctuation:
    two words joined by a maqaf), its override and
    alternatives go to every part, so indexes never shift. The same fields, each optional
    (null = inherit), plus `alternatives` (extra words ORed into that
    word's slot, searched with its options; if the word itself cannot
    search, e.g. `,`, its alternatives stand in for it). This is Otzaria's
    `wordOptions` / `alternativeWords`. `unmarked_alternatives` (C#
    `UnmarkedAlternatives`, Dart `unmarkedAlternatives`; low level
    `GroupSpec::Unmarked`) are searched the same way but their terms are
    not marked in the snippet unless a marked word of the search (main
    query or a phrase alternative) matched the same term: Otzaria's
    dictionary forms of a blacklisted lemma, which add hits and stay out of
    the highlight (issue #10). Costs nothing when unused; when used, those
    members and the marked ones are walked once more to tell their terms apart.
    The window is chosen as before (a slot matched only by an unmarked
    term still has to be in it), and only its marks leave those terms out.
    `lexical_forms` / `unmarked_lexical_forms` (C# `LexicalForms` /
    `UnmarkedLexicalForms`, Dart `lexicalForms` / `unmarkedLexicalForms`;
    low level `GroupSpec::Lexical`) are a word's dictionary forms from the
    host (Otzaria's MagicDictionary): exact words in its slot (each with its
    quote-inserted spellings, Otzaria's spelling terms), the second list not
    marked; under relevance order they score Otzaria's lexical tier (30).
    `max_expanded_terms` on a word (low level `GroupSpec::Capped`) is that
    word's own expansion cap instead of the request's.
  - Query level: `max_word_distance`, `require_ordered`, `min_groups`,
    `word_match` (`{"mode":"all"|"any_word"|"most_words"}` /
    `{"mode":"at_least","count":N}`: Otzaria's wordMatchMode, the engine
    counts the distinct slots it searches, a dropped word not counted and
    a repeated identical slot counted once; any mode but `all` applies no
    distance, order or proximity, even when it needs every word; wins over
    `min_groups`), `context_words`, `proximity` (per TYPED word, see the
    proximity item), `exclude` (`{query, words, phrase_alternatives,
    max_word_distance, require_ordered, proximity}`; options are per
    word: a negative word is a word like any other, the global options
    are its default and `exclude.words` its own overrides and
    alternatives, same typed-word index rule as `words`; no separate
    global set for the negative query),
    `books`, `phrase_alternatives` (`[{query, ignore_quotes}]`), `order`
    (`"catalog"` default, `"relevance"`, `"generation"`) and
    `book_generations` (`[[book_id, generation_order], ...]`) and
    `base_books` (`[book_id, ...]`, relevance only), see the result order item.
  - JSON: `{"query":"משה תורה","grammar_prefixes":true,
    "words":{"1":{"grammar_prefixes":false,"alternatives":["אורייתא"]}}}`.
  - The low-level `SearchRequest` (slots built by the caller) stays, for
    tests and special hosts; `SearchOptions` lowers to it.
- **Query groups** (the low-level slots): word (with options), wildcard,
  grammar (`%`), fuzzy (distance 1–3, FtsLib's range), and OR of
  alternatives.
- **Fuzzy** is hebrew-fts's own `HebrewFuzzyQuery` (the Levenshtein DFA
  tantivy uses, built here because tantivy's wrapper cannot be built from
  outside): edits are insert, delete, substitute and a swap of two
  adjacent letters as ONE edit (Otzaria's `transposition = true`; FtsLib's
  `Levenshtein.Distance` and FM `FindFuzzy` were changed to the same rule,
  optimal string alignment), cap `max_expanded_terms` (default 2000) across all segments, closest edit distance first,
  then shortest with the quote zero width, then most frequent. The plain
  query and the span slot are the same weight.
  - **`ignore_quotes`** (on by default in the FtsLib facade, like every other
    group kind): distances are measured on the quote-free forms, so the
    indexed יצי"ק is one edit from יצחק, as FtsLib's quote-free term יציק is.
    With the FM sidecar the candidates are confirmed with the same edit
    distance (swap = one edit) on the quote-free forms (FtsLib's
    `Levenshtein.Distance`); without it the DFA skips the quote byte.
  - **Distance 3** (FtsLib's maximum) runs through the FM sidecar; the walk
    path builds a distance-3 DFA, which is slower to build.
- **FM-index sidecar** (`<segment-id>.fmi`, a port of FtsLib's `FmIndex`):
  an FM-index over the term dictionary, built after the merge into one
  segment (`LineIndex::build_fm_sidecar`, C# `BuildFmSidecar` /
  `MergeSegments`, Dart `buildFmSidecar`), memory-mapped on first use.
  - It serves the shapes FtsLib served with it: leading-`*` wildcards of
    the form `*core` and `*core*` (no `?`, core of 2+ letters) and fuzzy
    words. Everything else stays an FST walk.
  - It finds candidates only: every candidate is verified with the walk's
    own automaton and capped with the walk's ordering, so the result is the
    same term list the walk gives (a test runs both paths and compares).
    Its text is the quote-stripped form of each term, so it works with the
    ignore-gershayim option; the verification restores exactness.
  - When the index has several segments (after incremental updates) or no
    sidecar, those groups fall back to the walk; merge and rebuild to get
    it back. On a `keep_marks` index the sidecar's text is the mark-free
    form too (the vocalized spellings of a word are rows of one text) and
    the candidates are verified on their mark-free bytes. Symbol codes are 16-bit and sort keys
    take the width the alphabet needs (every script being a word char, the
    dictionary holds about 280 distinct chars), as in FtsLib's `FmIndex`.
    The alphabet is the distinct chars present, chars outside the BMP
    included (issue #28): a BMP-only dictionary writes FtsLib's exact
    `FMI3` bytes; one with an astral char sets flag bit 1 and stores the
    chars table as u32 code points.
  - A sidecar that cannot be built is an error (C ABI -1 with the reason,
    C# / Dart exception; the C# and Dart demo builds print the failure as a
    build line, and the C# `build` report has the sidecar's time and
    size), never a silent skip. A sidecar file that exists but is corrupt,
    truncated or built over another dictionary is not used: leading-`*`
    and fuzzy words run as walks (same hits) and the search carries the
    notice `fm_sidecar_unusable` (delivered like `relevance_needs_positions`,
    C# `SearchInfo.FmSidecarUnusable`, Dart `SearchInfo.fmSidecarUnusable`)
    until the sidecar is rebuilt; this and the build error change bumped the C ABI to 5.
    The reason is read with `LineIndex::fm_sidecar_problem` (C ABI
    `se_index_fm_sidecar_problem`, C# `SeforimIndex.FmSidecarProblem`, Dart
    `fmSidecarProblem`; ABI 6). A broken file is remembered by its size and
    modification time and re-checked only when it changes, the index moves to
    another segment, or the sidecar is rebuilt (not re-opened per search).
    The sidecar is opened on a search's first leading-`*` or fuzzy word, single
    flight (issue #30): searches arriving together wait for ONE open and share its
    answer (ready or unusable); the wait stops on the search's cancel token, and a
    search with no such word never touches the sidecar.
    The engine mends the sidecar itself: a search that finds it missing or
    unusable on a one-segment index starts ONE background rebuild (a thread
    in the OS background mode; never while a `LineWriter` of the index is
    open, never two at a time, serialized with `build_fm_sidecar`). Searches
    keep the walk until the new file is published (temp file + rename,
    Windows sharing errors retried as in tantivy's atomic write), then the
    next search uses it. Once per problem: a failed rebuild (e.g. a
    read-only index directory) is recorded and not retried until the index
    moves to another segment or the host calls `build_fm_sidecar`.
    `fm_sidecar_problem` reports `rebuilding: <reason>` while it runs and
    `FM sidecar rebuild failed: ...` after a failure; Rust
    `LineIndex::set_fm_auto_repair(false)` turns it off and
    `fm_sidecar_rebuilding()` tells whether it runs (C ABI
    `se_index_set_fm_auto_repair` / `se_index_fm_sidecar_rebuilding`, C#
    `SeforimIndex.SetFmAutoRepair` / `FmSidecarRebuilding`, Dart
    `fmAutoRepair` / `fmSidecarRebuilding`; ABI 7). The rebuild thread is
    never joined: dropping the index sets a cancel flag the build checks
    between stages, so it stops promptly and never delays a drop or the
    process exit. Each build writes a temp file of its own
    (`<segment>.fmi.<pid>-<n>.tmp`) under an exclusive OS lock until it is
    published; a temp file left by a build that never finished (the app
    closed mid-build) is removed when the index is opened and before every
    build, while a locked one (a live build, in any process) or one younger than
    10 s (created, not locked yet) is left alone. A search
    cancelled during an FM expansion stops with `Cancelled`. The sidecar
    is used whenever it is present (no per-search switch).
  - Measured on the full index (1,755,598 terms): built in 2.4 s, 41 MB;
    `*יצח*` 41 ms against 1.1 s as a walk (FtsLib: 0.26 s).
- **Vocalized matching (nikud / te'amim)** (Otzaria's `match_nikud` /
  `match_taamim`; `match_nikud` / `match_teamim` in `SearchOptions`, globally
  or per word; C# `MatchNikud` / `MatchTeamim`, Dart `matchNikud` /
  `matchTeamim`). Needs an index built with `keep_marks` (an option of the
  build: the default index strips the marks and stays exactly as before;
  a vocalized search on it is an error).
  - The `keep_marks` index stores every term with its marks in canonical
    order and flags the lines that carry an attached mark (`marked`, one
    term: Otzaria's `contains_attached_marks` on the raw line).
  - Default search on it: every walk (word walks, wildcard, `%`, fuzzy
    DFA, user regex, FM verification) reads the marks as zero width
    (`hebrew_fts::marks::MarkFree`, a walk state around any term automaton)
    and the cap counts the mark-free forms (tantivy `ZeroWidthChars`: the
    vocalized spellings of a word are one slot, summed doc counts), so the
    lines are those of the mark-stripped index. The plain fast path stays
    a trie walk (inside the wrapper).
  - Matching (Otzaria's rule, its `vocalized_token_pattern`): every typed
    mark of an enabled class is required on its letter, every mark not
    typed is free, a typed mark of a disabled class and a typed combining
    accent are dropped. Classes as Otzaria: nikud U+05B0..U+05BC, U+05BF,
    U+05C1, U+05C2, U+05C7; te'amim every other attached mark (meteg
    U+05BD and the dots U+05C4 / U+05C5 included). The required marks are
    edges of the word walk's core trie, the free marks zero-width
    self-loops in every phase: affix letters and variants whose letters
    differ (ktiv, Aramaic endings, the typo list) take any marks; a typo
    edit drops the requirement (Otzaria's free-mark typo branches), a mark
    is never an edit.
  - Wildcard and fuzzy words take the options too (per word, like plain
    words; on a keep_marks index the query parser keeps their typed
    marks, the engine keeps the required ones). Wildcard (our design,
    Otzaria has none): the typed letters must carry the typed marks of an
    enabled class, every other mark is free, letters a `*` matches (and
    their marks) are free and marks never count against a `*` budget; a
    `?` drops its letter with that letter's marks; spelling variants of
    the core match mark-free. A mode of the wildcard automaton
    (`WildcardPattern::requires_marks`), so the FM sidecar verifies with
    the same automaton (FM == walk). Fuzzy (Otzaria's rule): the exact
    typed form requires its typed marks, the typo variants match mark-free
    (marks are never edits): the slot ORs the exact form's word walk
    (typed marks required) with the fuzzy DFA restricted to one edit or
    more (`LevenshteinDfa::min_distance`), on the walk and the FM path
    alike. With the options off the typed marks are dropped (as before).
  - Vocalized mode (Otzaria's `voc_mode`): when the flags are on globally
    or for any word, the search sees only lines that carry an attached
    mark, as Otzaria then runs the whole query on `textVocalized`, which
    holds only such lines (`hebrew_query.rs` `normalize_line_for_indexing`,
    `contains_attached_marks`; `search_engine.rs` `search_text_field`).
    A word with no typed mark then matches any vocalization, on those
    lines. The negative query is vocalized when the global flags are on
    (Otzaria passes them to both builds): it excludes only lines that
    carry a mark. Low level: `SearchRequest::vocalized`,
    `ExcludeSpec::vocalized`.
  - Deviations, named: marks are compared in canonical order (Otzaria
    requires the typed order; the order of two marks on one letter differs
    between texts); the 128-byte limit measures mark-free bytes (Otzaria's
    vocalized field drops a long vocalized word); the cap tie-break sums
    the doc counts of a word's spellings (FtsLib counted lines of its one
    stripped term: differs only when one line holds two spellings, at the
    cap boundary). The old exact-marks mode (`ignore_nikud` /
    `ignore_teamim`) is gone.
- **Partial matching** (Otzaria's wordMatchMode): `min_groups` in the
  request (C# `MinGroups`, Dart `minGroups`, demo `--match any|most|atleast`).
  A line needs only that many of the groups (anyWord 1, mostWords n/2+1,
  atLeast N, clamped to 1..n); it runs as a BooleanQuery with a minimum
  number of matching clauses, and the snippet is the tightest window
  holding that many groups. As in Otzaria (`scoped_words_query`), a
  partial mode (any / most / at least N) applies no distance, order or
  spacing even when N reaches the word count (it never falls back to the
  ordered query), and each distinct word counts once: the engine merges
  identical slots, and the Dart adapter merges a word typed twice into its
  first place (union of its options and alternatives).
- **Exclusion** (Otzaria's negative query): `exclude` in the request (C#
  `SearchRequest.Exclude`, Dart `exclude`, demo `--exclude "words"
  [--exclude-scope distance|paragraph|section]`): the words to drop, as a query of their own
  with its own distance, order and spacing, every word required (Otzaria's
  `apply_advanced_negative_query`, which builds it with the positive's
  builder). With no rule of its own (no `proximity`, `max_word_distance`
  or `require_ordered`) it takes the positive query's proximity, as
  Otzaria's negativeDistance defaults to its distance (issue #10): an
  any-order distance as is, an ordered rule with one distance for every
  negative pair; per-pair custom spacing is not inherited (Otzaria's
  negative custom spacing defaults to none). A host whose proximity carries
  a custom spacing (or a one-word query, which has no gap) passes
  Otzaria's plain distance and any order as `proximity_base`
  (`{distance, any_order}`; C# `ProximityBaseDistance` /
  `ProximityBaseAnyOrder`, Dart `proximityBase`): the negative default is
  then that distance for every negative pair, or that any-order distance.
  Without a proximity, the
  query's `max_word_distance` / `require_ordered` are inherited; the user's
  rule is inherited even when a whole quoted phrase replaces it for the
  positive query (Otzaria passes its distance then too). Options are PER WORD, as for the positive words: the global
  word options are the default, `exclude.words[i]` (`WordOptions`: any
  option, plus `alternatives` ORed into the word's slot) overrides them for
  negative word i (Otzaria's `negative_search_options` /
  `negative_alternative_words`); there is no separate global option set
  for the negative query (a host with one, like Otzaria's
  negativeGlobalSearchOptions, writes it onto every negative word). The
  words of a several-word negative query are phrase words (Otzaria's
  phrase budgets), as in the positive query. `exclude.phrase_alternatives`
  ORs whole phrases into the negative query (Otzaria ORs the acronym
  expansion of the negative words into it): a line carrying one is
  dropped, in the query (one word, or a span query with positions) or on
  the text without positions (adjacent, in order). One word, several words with no
  distance/order limit, or per-pair spacing are excluded in the query
  itself (MustNot); several words bound by distance/order are checked on
  the line text at locate time, like the positive filters. Section-scoped
  exclusion (Otzaria's sameSection) is the engine's: see the same-section
  scope item.
- **Phrase alternatives** (for Otzaria's ראשי תיבות): `phrase_alternatives`
  in the request (C# `PhraseAlternatives`, Dart `phraseAlternatives`). Each
  is a phrase ORed with the main query: its words adjacent, in order and all
  required (a span query with positions; without positions an AND checked on
  the text with FtsLib's distance rule, which lets a repeated word fill a
  gap). `locate` tries the main query, then each phrase, and marks the words
  of whichever matched. The acronym dictionary itself stays in the host.
  The Dart demo's lexicons (acronyms, Aramaic translation; `lexicon_text.dart`)
  fold gershayim / geresh to ASCII `"` / `'` and presentation forms in keys
  and values, and read keys sorted by code point as Otzaria does (serde_json
  BTreeMap), so the caps and reverse lists do not follow the file's order.
- **Morphology (Otzaria's MagicDictionary)** is host code, not engine code
  (issue #10): the Dart demo's `magic_dictionary.dart` ports
  `rust/src/magic/*` of otzaria_search_engine 0d7a33f step for step: opens
  a `lexical.db` read-only (demo `--lexical path`; Otzaria's downloader is
  app code, the host passes a path), the lookup keys and normalization,
  the selection (32 forms per word: 24 from a family whose base is the
  word, 8 from a family it is a surface of, 4 per family reached only
  through a spelling variant with a shared stem; forms under 2 letters, and
  variant-route surfaces under 3, dropped), the 512-entry cache and the
  hallucination blacklist (bundled as `magic_blacklist.dart`, generated
  from Otzaria's TSV). As in Otzaria only the fuzzy mode uses it: each
  index token's recall forms (the demo cuts typed words at the scanner's
  separators, maqaf included, so each word it sends is one index token),
  minus the token and its quote-free spelling, join its slot as
  `lexical_forms`, the forms of a blacklisted lemma as
  `unmarked_lexical_forms` (searched, not highlighted), and the request
  sets `lexical_fuzzy`: relevance then scores each fuzzy word as Otzaria's
  lexical fuzzy query (exact word 1000 + BM25, its quote-free spelling
  too, edit-distance terms 1, dictionary forms 30; several words: 2000
  when the typed words stand within one word in order). As Otzaria's fuzzy
  query, the demo's fuzzy mode sends no distance (words anywhere) and,
  without a dictionary, no quoted phrase. A whole query quoted (with a
  dictionary) is Otzaria's lexical fuzzy phrase: every word fuzzy with its
  forms, in order, at most 1 word between neighbours (ordered proximity,
  gaps 1), each word's edit-distance terms capped at 256 minus the word,
  its quote-free spelling and its forms (Otzaria's
  `MAX_LEXICAL_PHRASE_TERMS_PER_TOKEN`), scored as Otzaria's (one per word,
  the typed words' phrase BM25 and the 2000 adjacency boost, both slop 1).
  A quoted part of a longer fuzzy query (with a dictionary) is a required
  phrase in the engine: each word fuzzy with its forms and the same
  256-term budget, in order, at most 1 word between neighbours, the other
  words anywhere in the line; relevance scores per-word clauses for the
  unquoted words only, plus per phrase 1 per word and the typed words'
  phrase BM25 (slop 1), plus the 2000 boost over all typed words, as
  Otzaria's `build_lexical_fuzzy_query`. Otzaria's vocalized fuzzy
  forms do not arise (the demo's fuzzy mode carries no nikud option).
- **Otzaria word proximity** (`proximity` in the request, `ProximitySpec`;
  C# `SearchRequest.Proximity` = `Proximity.Ordered(gaps)` /
  `Proximity.AnyOrder(distance)`, Dart `Proximity.ordered` / `.anyOrder`;
  also on `ExcludeSpec` for the negative query). Otzaria's rule
  (`gap_phrase.rs` `Proximity`, `hebrew_query.rs` `resolve_proximity`):
  - `ordered { gaps }`: words in query order, at most `gaps[i]` words
    between word i and i+1 (Otzaria: `distance` for every pair, or
    customSpacing per pair). Gaps and `distance` are u32: `4294967295`
    is unbounded (same lines as one less); a value outside u32 (negative,
    fractional, larger) fails the search with an error.
  - `any_order { distance }`: every word at its own position, any order,
    all inside a window of `(k-1)(distance+1)` positions for k words.
    A repeated query word needs its own occurrence.
  - With positions: one span query (gaps, or window mode), the rule checked
    on each candidate's positions. Without positions: the AND of the words,
    and the snippet locator validates the same rule on the line's tokens
    (the index positions ARE the locator's tokens: same scanner), in the
    one tokenize + locate pass, reusing its per-token term numbers. Both
    index flavours return the same lines and the same windows
    (`proximity_positions_equal_text_validation`).
  - Snippet: the tightest window (raw chars, ties = earliest) that
    satisfies the rule; marks = every query-term token in the window.
  - Precedence: when set it replaces `max_word_distance`,
    `require_ordered` and `gaps` for the main query (FtsLib's rule, kept
    unchanged when it is None); `min_groups` below the group count wins
    over it (as Otzaria); every word must be searchable, else no hits.
  - In `SearchOptions` (and its `exclude`) the proximity is given per
    TYPED word: `gaps[i]` between typed words i and i+1, one entry per
    pair, dropped words included. A one-letter word or digit run is a
    word: a slot, checked at its own position, exactly as Otzaria
    (`resolve_gaps`). A word the engine drops (no letters or digits, e.g.
    `,`) has no position in the text either, so it is cut out and the gaps
    around it merge: `a x b` with `[g0, g1]` -> `g0 + g1` words between `a`
    and `b`. A word of 128 bytes or more keeps its one position, as in the
    text: it leaves the query (no term) and `a x b` -> `g0 + 1 + g1`.
    Deliberate deviation (user's choice): Otzaria returns no hits for such
    a query (its literal matches no term); we keep the slot and search the rest.
    Without a proximity it stays a skipped slot (`GroupSpec::Unindexed`)
    that counts toward word distance, as FtsLib's skipped wildcard. Any
    order: Otzaria's window `(n-1)(distance+1)` for its n words, minus one
    per dropped or too long word (each took one of its positions), sent as
    `window { max_span }`. Sub-words of one typed word are adjacent (gap 0);
    an OR slot takes the gap before its first word and after its last. A
    dropped word inside an OR slot is still counted as a dropped word
    (Otzaria has no `|`; the Otzaria demo turns `|` into a space).
  - Hosts that number the query's words themselves (per-word options,
    per-pair spacing) first reduce the paired readings with the engine's
    own rule: C ABI `se_reduce_paired_readings`, C#
    `SeforimIndex.ReducePairedReadings`, Dart
    `SeforimIndex.reducePairedReadings` (the Dart demo does).
- **Per-pair spacing** (FtsLib-style request field, positions only): `gaps` in the request (C#
  `SearchRequest.Gaps`, Dart `SearchRequest.gaps`, demo `--spacing 2,5`).
  - `gaps[i]` is the maximum number of words between query word i and i+1.
  - Every group kind works as a slot; word order follows `RequireOrdered`.
  - It runs as a span query, so it needs an index with positions; otherwise
    the engine returns a clear error. The positions count every word
    (one-letter words and digit runs too).
  - Measured on the full index, בית המקדש adjacent: 15,218 lines in 29 ms.
- **Result order** (Otzaria's `ResultsOrder`; `order` in `SearchOptions`,
  C# `SearchOptions.Order` / `BookGenerations` / `BaseBooks`, Dart `order` /
  `bookGenerations` / `baseBooks`, Otzaria demo `--order catalog|relevance`;
  `seforim-engine/src/relevance.rs`). Catalog order stays the default and
  costs nothing: no score, no doc id kept.
  - `generation`: Otzaria's `OrderKind::Generation`: books by the
    generation order the host gives per book (Otzaria's app pushes its
    `generationOrder`; the engine keeps no generation), then catalog order.
    A book without one comes last.
  - `relevance`: Otzaria's `order_by_score()` over the query Otzaria builds
    for the same search, computed after the search for the hits only, from
    the terms the walks matched (sinks). Sort key (issue #6): the tier
    (the score's constant boosts: tier boosts, fuzzy floors, word counts),
    then base books first (`base_books`: the host passes seforim.db's
    `book.isBaseBook` ids per search, nothing in the index; both demos read
    them), then the rest of the score (BM25 / span bonus), then catalog
    order (Otzaria: ascending doc address). A base book never crosses a
    tier. Per shape:
    - one slot: Otzaria's tiers (`match_tier`, ported verbatim: the typed
      form or an alternative 0, inside a longer word 1, a ktiv male/haser
      variant 2, the rest 3), a line scores the sum of 1000 / 100 / 10 / 1
      over the tiers it holds (on a keep-marks index a term tiers by its
      mark-free form). A vocalized search (`match_nikud` /
      `match_teamim`) is Otzaria's vocalized field, which it never tiers:
      1 per line.
    - a fuzzy slot (`word~N`, Otzaria's fuzzy mode, its plain builder):
      1000 + BM25 of the exact word (its spellings: the word and the terms
      that are the word with quotes inserted, frequencies summed, doc counts
      shared), the same for the word without its quotes, + 1 for the fuzzy
      match; several fuzzy words: + 2000 when they stand within one word of
      each other.
    - several slots, ordered proximity or per-pair `gaps`: BM25 of the
      phrase with tantivy's own `PhraseScorer`, slop = the sum of the gaps
      (Otzaria's `RegexPhraseQuery` + `GapVerifiedPhraseQuery`). Tested equal
      to tantivy's `PhraseQuery` scores.
    - several slots in any order: the slot count + 1 / (1 + the tightest
      span), every slot at its own position (`TermListPhraseQuery`).
    - several slots, words anywhere in the line (FtsLib's AND, Otzaria's
      paragraph scope) or partial matching: 1 per slot the line holds
      (Otzaria's per-word `TermSetQuery`s); a fuzzy slot scores as above.
    - per-pair `gaps` in either order (`require_ordered` false; no Otzaria
      counterpart): as any order.
    - each phrase alternative (acronyms): its phrase BM25 (1 for one word);
      when the main query is several words anywhere, 1 per word, all
      required (Otzaria builds the alternative in the main query's
      paragraph scope; no positions needed then). Added to the main
      query's score only for lines the main query matches too (a copy of
      the main query is run for that, its walks again: relevance with
      alternatives only).
  - Index data: none added. Tiers and counts need only the postings (both
    flavours); fuzzy BM25 uses term frequencies and field norms (without
    positions there are no frequencies: every frequency reads as 1, as for
    Otzaria's own frequency-less terms); phrases, spans and the fuzzy
    adjacency need positions (adding only frequencies would not serve them:
    about +170 MB, the `.idx` difference of the two full builds). On an
    index without positions those shapes do not fail: the search comes
    back in catalog order (no scores) with the notice
    `relevance_needs_positions` (Rust `SearchResult::notices` / `info`, C
    ABI `se_search_info` JSON `{"notices":[...]}`, C#
    `SearchSession.GetInfo().Notices` /
    `SearchInfo.RelevanceNeedsPositions`, Dart `session.info().notices` /
    `SearchInfo.relevanceNeedsPositions`; the C# demo `count --order
    relevance` and the Otzaria demo print it).
  - Deviations (named): the phrase IDF is taken from the slots' typed words
    (Otzaria's from its regex pattern strings, which equal the word for a
    plain word): a constant factor per query, the order inside a query is
    the same. Otzaria's flat-scored degrade path (phrases whose expansions
    pass its ceiling) is not ported: our walks cap instead of failing, so
    every phrase is scored. Otzaria's magic dictionary forms reach the
    engine from the host as `lexical_forms` and score its lexical tier
    (30); `lexical_fuzzy` gives the fuzzy words its lexical query's score
    (see the morphology item). In a vocalized
    search fuzzy words score 1 (Otzaria's vocalized fuzzy is unranked), and a
    fuzzy word at distance 0 scores 1 (Otzaria adds its quote-free
    spelling's BM25). A repeated word is a slot of its own in the count
    (Otzaria merges duplicate words). A one-word query's multi-word
    alternative scores as a phrase (Otzaria's default word-distance scope).
    FtsLib's `max_word_distance` /
    `require_ordered` without a proximity keep the count score (Otzaria
    would build a phrase from its distance). The book filter adds nothing to the score
    (Otzaria's facet clause adds its own BM25). A generation-ordered book
    without a generation follows the catalog order (Otzaria: its book
    number).
  - Cost (full index, release, median of 3 after a warm-up; engine search
    only): word with prefixes, 493,948 hits: 107 ms catalog / 203 ms
    relevance (no positions 104 / 262 ms); phrase gap 0, 15,228 hits: 32 /
    38 ms; any order: 32 / 47 ms; `word~1`, 599,323 hits: 95 / 243 ms; two
    words anywhere: 22 / 30 ms.
- **Expansion cap per request** (issue #5): `max_expanded_terms` on
  `SearchOptions` / `SearchRequest` / the C ABI JSON, C#
  `SearchOptions.MaxExpandedTerms` (demo `TantivySearchOptions.MaxExpandedTerms`),
  Dart `SearchOptions.maxExpandedTerms`. How many index terms one query word
  (walk, wildcard, fuzzy, `%`, regex word, FM-sidecar expansion) may expand
  to. Default 2000 (FtsLib's, parity); any value is used as is; 0 = uncapped.
  ONE cap across all segments: the segments' walks run together (doc counts
  summed), so the kept terms and hits do not depend on the segment layout
  (free on the merged index). The exact typed word and its quote-free twin
  always keep a slot, then shortest first. The line regex keeps its own
  limits.
- **Truncation flag** (Otzaria's `truncated`): a search reports when any
  of its walks (a word, an alternative, a negative word, the FM-sidecar
  expansions too) hit its expansion cap, so matching
  terms past the cap were not searched. Rust `SearchResult::truncated`,
  C ABI `se_search_info` (`"truncated"`), C# `SearchInfo.Truncated`, Dart
  `SearchInfo.truncated`; both demos print it. Cost: none when no cap is
  hit (tantivy's `MatchedTermsSink::is_truncated`, set by the capped walk;
  only a cap filled exactly at the end of a length level searches depth
  first for one more match, stopping at the first; with none left it may
  visit the rest of the matching subtree).
- **Snippets from the index positions** (`SnippetSession::try_locate_lines`,
  C ABI `se_search_locate_lines`, C# `SearchSession.LocateLines`, Dart
  `SearchSession.locateLines`): hits are given as their LINES, each hit its
  matched line (optionally further lines, each with its `line_id`, plus the
  hit's index in the hits when known). The engine joins the non-empty texts
  by single spaces and, on an index with positions,
  takes the matched words from the postings instead of hashing every word
  of the text. See "Snippets from positions" below. The text-only calls
  (`locate_batch`, `locate_pairs`) stay: an index without positions, the
  line regex and cross-line pairs use them.
- **C ABI conventions:** objects are opaque handles; strings cross as UTF-8
  pointer + length; errors are `0/-1` status codes with a per-thread
  `se_last_error` (cleared at the start of every call, never seen by
  another thread); hot paths are binary (no JSON). Text the engine hands
  back that may hold any character (`se_reduce_paired_readings`) is
  pointer + `out_len`, freed with `se_bytes_free`, so an embedded NUL
  survives; JSON results stay null-terminated (JSON escapes a NUL). Ids
  (`line_id`, `position`, `book_id`, `section_id`) are u64 in the ABI with
  the domain 0..=i64::MAX (SQLite rowids, C# `long`, Dart `int`): a larger
  value (a negative C# / Dart id) is rejected at ingest; u64::MAX means
  "none" in outputs. `SeBuffer` is three pointer-sized words on every
  target (compile-time assert, 32-bit included). Located marks carry a
  term id into the search's term table; `se_search_take_new_terms` (C#
  `TakeNewTerms`) returns the terms added since the previous call.

#### Threading and writers (issue #31)

- **An index object is shared by threads.** Searches, counts, `se_index_reload`,
  info and sidecar checks on one `LineIndex` (C ABI index pointer, C#
  `SeforimIndex`) may run concurrently from any number of threads (Rust:
  `LineIndex: Send + Sync`, asserted in a test). Dart: an isolate opens its own
  `SeforimIndex` (handles do not cross isolates); the async API does so itself.
- **Every other object is one thread at a time:** a writer, a search session
  (`se_search*` result), a cancel token's owner. Use one per thread, or a host
  lock around its calls. Different sessions are independent: each keeps the
  snapshot it started with, whatever other sessions or the writer do.
- **One writer per index directory.** A second writer, on the same handle, on
  another handle opened on the same directory, or in another process, fails at
  open with tantivy's lockfile error: Rust `Error::Tantivy(LockFailure(LockBusy, ..))`,
  the C ABI returns null from `se_writer_open` / `_open_ex` and `se_last_error()`
  holds `Failed to acquire Lockfile: LockBusy. ...` (the message contains
  `LockBusy`), C# `OpenWriter` throws `SeforimEngineException`, Dart `openWriter`
  throws `SeforimEngineException`, both with that message. The first writer is not
  affected; once it is closed (or its process exits) a new writer opens. Tested in
  all four layers.
- **Reader freshness:** a commit, rollback or merge through this handle's writer
  reloads the shared reader before it returns, so the next search on this handle
  sees it (a running search keeps its snapshot). A write through ANOTHER handle or
  process is seen after `se_index_reload` (C# `Reload()`, Dart `reload()`) or a reopen.
- **Rollback** drops everything queued since the last commit; searches never saw
  it. A crash or kill before a commit loses only the uncommitted queue; the last
  commit stays intact.
- An index with no segment yet (created, nothing committed) answers every search
  with no hits.
- Load behaviour (concurrent searches while a writer commits) is measured by the
  benchmark's load mode (`search_report.py --load`, below).

#### Writer resources (issue #19)

Goal: use as much of the machine as possible while it is idle, without the
user ever noticing the build. The writer's **auto mode** (`LineIndex::writer_auto`,
`SE_WRITER_AUTO`, the SDKs' and both demos' default; fixed heap / threads
stay available):

- **Threads:** one per core (capped so each thread's 15 MB minimum fits in a
  quarter of the free RAM), all in the OS background mode (tantivy item 6),
  so the OS throttles CPU, disk and memory for us. The host's feeding thread
  opts in with `se_thread_background_begin` / `_end` (C#
  `using (BackgroundPriority.Enter())`, Dart `runInBackgroundPriority`; set
  natively, never .NET `Thread.Priority`). Commits, the final merge
  (`merge_all`, an explicit merge) and the FM sidecar build run at normal
  priority: a reader waits for them, and a throttled single-threaded tail
  drags.
- **Memory, event based** (`seforim-engine/src/resources.rs`): one small
  watcher thread per auto writer blocks on the OS signal (about one wakeup
  per second) and moves the writer's live budget in three tiers: under memory
  pressure the minimum (15 MB per thread); while the user is active (no
  pressure) a moderate 64 MB per thread, capped so all threads take at most an
  eighth of the free RAM (never below 15 MB); idle, a quarter of the free RAM
  shared by the threads (at most 512 MB per thread). A worker above a lowered
  budget flushes its segment at its next batch and frees its buffer. The
  moderate value is the knee measured with `writer_resources_bench` (300k
  synthetic lines, 8 threads): 15 MB/thread 0.7-2.2 min and 67-85 segments,
  32 MB 0.4-0.6 min, 64 MB 0.1-0.5 min and 8 segments, 128 MB 0.1-0.2 min;
  peak process memory 160 MB at 15 MB/thread, 320-380 MB at 64 MB (busy machine,
  wide spread).
  The writer reads one atomic per batch, never per line.
  - Windows: `CreateMemoryResourceNotification(LowMemoryResourceNotification)` +
    `WaitForSingleObject` (1 s), `GetLastInputInfo` (input in the last 10 s =
    active; per session), free RAM `GlobalMemoryStatusEx`
    ([1](https://learn.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-creatememoryresourcenotification),
    [2](https://learn.microsoft.com/en-us/windows/win32/api/synchapi/nf-synchapi-waitforsingleobject),
    [3](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getlastinputinfo),
    [4](https://learn.microsoft.com/en-us/windows/win32/api/sysinfoapi/nf-sysinfoapi-globalmemorystatusex)).
  - Linux / Android: PSI trigger `some 150000 2000000` on `/proc/pressure/memory`
    polled for `POLLPRI` ([doc](https://docs.kernel.org/accounting/psi.html)),
    free RAM `MemAvailable` ([doc](https://man7.org/linux/man-pages/man5/proc_meminfo.5.html));
    no user-activity signal (none is generic).
  - Apple platforms: no watcher yet (the GCD memory-pressure source needs
    blocks from Rust); the budget stays as opened (64 MB per thread).
- Linux: leaving `SCHED_IDLE` needs `RLIMIT_NICE` headroom (often 0), so
  without it the CPU side is `SCHED_BATCH` (reversible, weaker); the idle I/O
  class applies either way.
- Pools (`seforim-engine/src/pools.rs`): `indexing_pool()`, a lazily built rayon pool,
  one thread per core, each worker in the OS background mode for its life (start /
  exit handlers), for the engine's own parallel indexing-side work; a normal-priority
  search pool joins it with issue #24. No indexing-side code is parallel yet (the FM
  sidecar build is single-threaded and runs at normal priority, above).
- Micro benchmark: `cargo run --release -p seforim-engine --example
  writer_resources_bench -- [lines] [rounds]` (auto vs fixed, watcher
  overhead, simulated pressure).

### C# demo (`csharp/SeforimSearch.Demo`)
- **DB code**, copied from KitveiHakodesh Core with namespace renames only:
  - schema probe (3 DB shapes), zstd codec, catalog tree order, content
    fingerprint
  - corpus reads, and `SeforimDbFtsSearcher` (batch plan, every hit keeps the
    descriptor its check located, `SnippetFlagShort`, book id read for passing
    hits only, no TOC entry on hits; the first 100 hits' `FtsSnippetText`s
    seeded, short ones with their ±2 neighbour context from
    `FetchSnippetTexts`: plain ids by primary key and short ids with their
    neighbours in ONE statement)
- **FtsLib facade with the same names:** `FtsDocumentPlacement`,
  `SearchResult`, `SnippetDescriptor` (the query string goes to the engine
  as `SearchOptions`; FtsLib's `QueryParser` is ported in hebrew-fts), and
  `TantivySeforimIndex` (`Search`, `LocateSnippetWindow`, resumable
  `BuildLines`, `GetResumeState`). Batches go to the engine as lines
  (`LocateLines`: each result's hit index and line id), so a positions index
  locates them from the postings.
- **Footnotes as virtual notes books** (issue #13, all three hosts: C# demo,
  Dart demo, KitveiHakodesh Core / FtsLib, same order and ids, so parity
  holds). On a schema-7 DB (`line_note` table) each book with notes is
  followed in catalog order by its notes book; each `line_note` row is one
  line of it, in source order (source `lineIndex`, line id, `noteIndex`), no
  section, never joined across lines (no-cross). Ids live in a reserved range
  that real ids can never enter, by construction (helper `SeforimDbNoteIds` /
  Dart `note_ids.dart`):
  `R = 2^52`; virtual line id = `R + (lineId << 16) + noteIndex` (needs
  `lineId < 2^36`, `noteIndex` 0..65535; max `2^53 - 1`, exact as a JavaScript
  number; the engine's u64, C# `long`, Dart `int`); virtual book id = `R + bookId`.
  Every line / book id a virtual id can reach is 64-bit in the hosts (index
  format `tantivy16`). Decoding gives back
  (source line id, note index) and the source book. The index build fails
  loudly, naming the id, if a real `line.id` / `book.id` reaches `R` or a note
  is out of range (local DB: max line id 10,350,062, max noteIndex 54). The
  index stays bare minimum: the host reads the note body from `line_note` by
  the decoded id (same codec), for indexing and for the snippet, which is the
  note alone. Hits expose `IsNote`, `SourceLineId`, `NoteIndex`,
  `SourceBookId` (Dart `noteSource`), so the app opens the source book at that
  line. A DB without `line_note` is unchanged.
- **Rows are tri-state** (issue #13): a line's text is found, missing or
  unreadable. On the search path an undecodable row reads as empty and is
  recorded (line id, first error naming it, issue #8), and the search result
  carries the count (`UnreadableRowCount` / `UnreadableLineIds`; Dart
  `unreadableLineIds`; the demos print "N rows unreadable"), so 0 hits is
  never silently wrong. Indexing stays strict. One leading U+FEFF is stripped
  when a row or a note body is decoded. Rows stored as UTF-16: none on the
  local DB (all 7,660,127 rows decode as UTF-8, no NUL), so none handled.
- **Dart demo** (`dart/otzaria_demo`) does the same: no widening; short rows
  get plain ±2 neighbour context from `SeforimDb.snippetTexts` (one statement
  per page), rendered as the KitveiHakodesh page does (`withContext`).

## Build and search reports

Full comparison on any machine (index builds, parity, speed, Otzaria hit counts, report) with one command: [tools/bench/README.md](tools/bench/README.md) (`tools/bench/run-all.sh`). Its small result files are committed in [bench-results/](bench-results/README.md); `python -I -X utf8 tools/report/specs_report.py` (no argument) regenerates the specs docs from them.

- `SeforimSearch.Demo build` prints progress, then each stage's time
  (indexing, merge into one segment, FM-index sidecar), the total, and the
  index size in total and per file type.
- `python -I tools/report/search_report.py <demo exe> <index> <queries> <out> [--runs N]`
  runs every query with the engine's stage timing on and writes
  `search-report.md`: per query the engine hits and the time of each stage
  (query weight; FST walks / FM lookups + hit collection; host text fetch +
  snippet locate + seeded short-row context), time to the first batch, total, and exit code. Every run's full
  log is kept next to the report. `--runs N` adds one warm-up run, then
  reports each stage as the median of N runs (every run in `search-<n>.log`).
- Load mode (issue #31): `python -I tools/report/search_report.py <demo exe> <index> <queries> <out>
  --load N [--db <seforim.db>] [--rounds R] [--commit-every MS] [--update-lines K]` copies the
  index to `<out>/load-index`, runs `SeforimSearch.Demo load` (N concurrent searcher threads, each
  running every query R times through the streaming path, while one writer re-indexes K lines,
  delete + add of the same documents, and commits every MS ms; defaults 3 / 1000 / 100), and writes
  `load-report.md` / `.json` (per query first-batch and total p50 / p95 / max, hit counts stable;
  commit latency p50 / p95 / max) plus the full `load.log`. In the full benchmark it is the `load`
  step (`LOAD_*` in `tools/bench/bench.env`); its section in FTS-SPECS-AND-COMPARISONS.md says
  "not run yet" until it is run on the full DB.

## Match mode: all matches, and find in an open text (issues #14 / #23)

The snippet locator (`SnippetWindowLocator`, FtsLib's too) has a match mode
(`MatchMode` / FtsLib `SnippetMatchMode`):

- **Best window** (default, unchanged): the tightest window and its
  occurrences' marks.
- **All matches**: for a text that passes, the window is the whole text
  (`start` 0, `end` its UTF-16 length, no ellipsis) and the marks are the
  tokens of EVERY full occurrence in the text, by the best window's own rule
  (issue #4: FtsLib distance / order, Otzaria proximity, phrases, a
  cross-line occurrence crossing the break; partial matching and no distance:
  every query-term token), with the same word rules (HTML, entities, marks,
  quotes, paired readings). Which hits
  pass, `score`, `word_distance`, the order and the counts do not change.
  Session API: `SnippetSession::set_match_mode`, C ABI
  `se_search_set_match_mode(search, 0|1)` (applies to `se_search_locate_batch`
  / `_lines` / `_pairs`), C# `SearchSession.MatchMode`, Dart
  `SearchSession.matchMode`. All matches locates from the text (the positions
  path stops after the window, so it is skipped); a line-regex search keeps
  its own snippet. FtsLib: `Locate(..., matchMode)`,
  `LocateSnippetWindow(..., matchMode)`; KitveiHakodesh Core:
  `SeforimDbFtsSearcher.SearchInBatches(..., matchMode)`.
- **Find in an open text** (no index, no session; a host's find-in-book on
  an open page): `SnippetWindowLocator::find_all_matches` with
  `PreparedQueryGroups::from_text_words` (each word normalized by the line
  scanner), C ABI `se_find_all_matches(text, words, flags)` (flags
  `SE_FIND_PHRASE` = 1: only runs of the words in order at consecutive
  positions, overlapping runs included; `SE_FIND_IGNORE_QUOTES` = 2;
  `SE_FIND_MATCH_MARKS` = 4: nikud / te'amim as typed, else both sides drop
  them), C# `TextMatcher.FindAllMatches`, Dart `findAllMatches`, FtsLib
  `Fts5SeforimIndex.FindAllMatches` (same options; FtsLib's terms are
  quote- and mark-free, so it checks a candidate's surface form, quotes and
  canonical marks, read off its raw span). Output: (start, length, term ordinal) triples,
  UTF-16, document order; a mark ends at the next visible char (a closing
  inline tag right after the word is inside it), as every locate's marks.
  Literal words only (no wildcard / grammar / fuzzy expansion: those walk an
  index dictionary).
- Quotes as the walks read them (Otzaria's QuoteTransparent; our terms keep
  quotes): a quote-free word finds the quoted token, a typed quote needs it
  unless ignore-quotes; the same in both engines (tested on both sides).

Speed (`cargo run --release -p hebrew-fts --example find_all_bench`, a
1,048,579-unit book-like page: vocalized words, tags, entities, quotes;
one call per page, best of 30; this machine was shared with other builds,
so absolute numbers are noisy, ratios hold): one word (20,311 matches): Rust 33.6 ms; from Dart through FFI 47.0 ms, the
same page with Otzaria's Dart literal-regex approach (letters with optional
marks and inline tags, word-boundary lookarounds; ported as a scratch
benchmark) 77.9 ms in that run (74 ms vs 199 ms in a busier run); from C#
54 ms (.NET 10) / 61 ms (.NET Framework 4.8), encode + FFI + copy
included. Two-word phrase: Rust 49 ms. Issue #23's own figures (another
machine): Dart 22.9 ms, the old Rust prepared matcher 62.5 ms. The default
best-window locate is unchanged (`locate_bench` before/after, alternated:
full locate 38.7 vs 39.0 µs per 2.2 KB line, within noise).

## Snippet-locate speed

The host loop that reads every hit spends most of its engine time locating
snippets. Measured single-thread on 2.2 KB HTML lines
(`cargo run --release -p hebrew-fts --example locate_bench`): the scanner
went from ~100 to ~380 MB/s and the full locate from ~70 to ~250 MB/s, with
byte-identical output (the old scanner is kept as a test-only reference and
compared on ~65 edge cases plus 20,000 random strings). `locate_many` splits
work by bytes across reused per-thread locators. An 802,736-hit search read
in full went from 54.8 s to 29.0 s (C# demo, no-positions index).

## Snippets from positions

On an index with positions the engine already knows at which word
positions of a line the matched terms sit, so `locate_lines` takes them
from the postings instead of hashing every word of the text:

1. Each line's doc: a hit's (segment, doc) is kept from the search
   (aligned with the hits, carried through the sort); any other line (a
   neighbour) is found by its `line_id` term. Deleted docs are skipped.
2. Per batch and segment, every matched term of the session (the sinks'
   terms of the main query, the phrase alternatives and the text-level
   negatives, numbered once) is read from the postings ONCE, seeking
   through the batch's sorted docs (split across cores by term and by doc
   range on large batches). Each line gets its sorted `(position, term)`
   list; a term repeated at one position (a paired reading `(X) [X]`) is
   one entry.
3. One light scan of the text (`SnippetWindowLocator::tokenize_positioned`):
   the same scanner (word boundaries, positions, UTF-16 and visible
   offsets), but no normalized text is built for plain letters and no word
   is hashed; a word's text is read only where a matched term sits, to
   check the term is that word (paired readings share a position). The
   scan stops once the words after the last matched position can no longer
   reach a window or its context.
4. The window rules (FtsLib's tightest window, ties to the earliest, marks,
   distance, order, proximity, partial matching, negatives, phrase
   alternatives) are the SAME code as the text path, fed the term ids
   through a per-matcher map, so the snippets are byte-identical.

The window is chosen after the scan, not before: FtsLib scores a window by
its span in characters, which positions alone do not give. A text that
disagrees with its positions at a matched word (a line the index does not
hold, a text that is not the indexed one) is located from the text. The
texts given must be the indexed lines' texts: a text changed only away
from the matched words is not detected (the index is stale then anyway).

**When it is used.** Reading postings costs per matched term and per
posting block; the work it saves grows with the line's length. Measured on
the full library, it pays off on long lines with up to a few hundred terms
and loses on short lines (a common single word) and broad expansions (a
capped walk, a word with all its prefixes). So a batch takes the positions
only when its texts average more than `1500 + 2 x terms` bytes
(`PositionSource::pays_off`); otherwise the text path, same snippets.

Measured (`cargo run --release -p seforim-engine --example positions_bench`,
full-index-v8, the first 4000 hits of each query with their seforim.db
texts, 8 cores, median of 15 runs; the same snippets checked on every run):

| query | terms | avg line | text path | from positions | change |
|---|---|---|---|---|---|
| two words | 16 | 2444 B | 23.1 ms | 19.5 ms | -15% |
| two words, distance 0 | 16 | 2444 B | 22.7 ms | 18.7 ms | -18% |
| two words, proximity | 21 | 2713 B | 24.6 ms | 19.9 ms | -19% |
| wildcard `*x*` | 245 | 2013 B | 19.8 ms | 15.1 ms | -24% |
| fuzzy ~1 | 75 | 2100 B | 20.0 ms | 15.3 ms | -23% |
| three words | 22 | 1271 B | 13.0 ms | 13.2 ms | text path |
| one common word | 11 | 627 B | 6.2 ms | 6.6 ms | text path |
| word with prefixes+suffixes | 462 | 1473 B | 13.1 ms | 13.0 ms | text path |
| capped wildcard `xy*` | 3031 | 293 B | 3.7 ms | 3.7 ms | text path |

In batches of 256 (FtsLib's first batches) the winners gain 10-15% (two
words, distance 0: 35.7 -> 31.5 ms for 16 batches) and the wildcard breaks
even. The first run of a process (the positions file first touched) shows
the same gains (two words: 23.1 -> 19.8 ms); a cold OS file cache was not
measured. Forcing the positions everywhere (`force_positions`) makes the
losers slower: one common word +23%, three words +14%, the capped wildcard
+117% at 4000 lines.

## Hits decided from positions (issue #21)

On an index with positions every search decides each hit during the
search, from the postings: the occurrences of the session's matched terms
in a chunk of hits are read as for "Snippets from positions" (step 2), each
hit's `(position, term)` list is loaded into the locator as tokens
(`SnippetWindowLocator::tokenize_occurrences`, no text) and the session's
own matchers and filters run on it (`passes_tokenized`: the FtsLib distance
and order, Otzaria's proximity, partial matching, phrase alternatives,
text-level negatives and required phrases; a vocalized negative reads the
`marked` line flag's postings). A hit that fails is dropped before the host
sees it, so the totals and per-book counts are exact without text and any
page (hit 100 000) is there at once. When nothing is left to check (one
word, a plain AND, spans already on positions) no position is read: the
hits pass as they come. The hits are streamed (see "Cancellation and
streaming"). Code: `seforim-engine/src/stream.rs`.

Cross-line matching on an index with positions decides every candidate pair
on the joined positions (`cross_line.rs`: `L` up to its last content word,
`L + 1` from its first content word, from the `line_last` / `line_first`
fields; `SnippetWindowLocator::tokenize_occurrences_across`), running the
way of matching's own matcher across the break: FtsLib's distance and
order, a cover across an edge enumerator and Otzaria's proximity alike. So
the cross-line hits are exact too, and a hit with `next_line_id` passes the
in-line decision (its pair already passed).

The window is the word-tightest one (`SnippetWindowLocator::set_word_span`,
the user's decision): a session on an index with positions picks, on the
displayed rows' texts too, the window spanning the fewest word positions,
the earliest on a tie, so the snippet shows the same occurrence the search
decided on. Text is read only for the rows displayed (UTF-16 offsets of the
window and marks). Nothing per hit is kept beyond the hit and its doc: the
window is a pure function of the positions and is found again for a
displayed row at no extra cost. A deviation from FtsLib, named under
"Differences from FtsLib". An index without positions keeps FtsLib's path
step for step (character-tightest window, the text decides).

Measured on the full library (`tantivy16-positions`, this machine, medians
of 5 runs after a warm-up, one process per query; before = `eefabda`):

| query | hits before -> after | first batch | complete (exact) | hit 100 000 | peak private memory |
|---|---|---|---|---|---|
| one common word (2 letters) | 1,123,609 | 219 -> 11 ms | 219 -> 150 ms | 219 -> 28 ms | 139 -> 152 MB |
| one very common word | 2,005,892 | 253 -> 12 ms | 253 -> 188 ms | 253 -> 27 ms | 179 -> 165 MB |
| prefix wildcard | 481,974 | 70 -> 8 ms | 70 -> 50 ms | 70 -> 22 ms | 49 -> 51 MB |
| fuzzy ~1 | 140,677 | 38 -> 9 ms | 38 -> 30 ms | 38 -> 30 ms | 24 -> 25 MB |
| narrow word | 4,053 | 7 -> 7 ms | 7 -> 8 ms | - | 7 -> 9 MB |
| two words, plain AND | 15,569 | 16 -> 7 ms | 16 -> 11 ms | - | 10 -> 10 MB |
| two words, distance 5 | 15,569 -> 7,418 | 11 -> 8 ms | text (11 ms + 409 ms locate + fetch of 15,569 texts) -> 28 ms | - | 9 -> 14 MB |
| two words, in order, distance 2 | 155,198 -> 78,061 | 43 -> 12 ms | text (43 ms + 2360 ms locate + fetch of 155,198 texts) -> 163 ms | text (~100k texts) -> none | 25 -> 33 MB |
| three words, distance 10 | 14,003 -> 835 | 22 -> 11 ms | text (22 ms + 516 ms locate + fetch) -> 35 ms | - | 11 -> 16 MB |
| two words, proximity any order (span query) | 7,417 | 29 -> 12 ms | 29 -> 24 ms | - | 15 -> 16 MB |
| two words, proximity in order (span query) | 78,061 | 101 -> 13 ms | 101 -> 86 ms | - | 22 -> 22 MB |
| three words, proximity | 2,008 | 59 -> 28 ms | 59 -> 39 ms | - | 18 -> 18 MB |
| quoted phrase (cross-line) | 67,664 | 390 -> 32 ms | 390 -> 294 ms | - | 56 -> 41 MB |
| two words, distance 5, cross-line | text -> 7,583 | text -> 21 ms | text (locate + fetch of every candidate pair) -> 84 ms | - | -> 21 MB |

Before, "first batch" = "complete" = the whole search (every hit
collected). A cross-line search's chunk runs the cross-line pass on its
own lines only (`CrossInput::range`: the slots' postings read from the
chunk's first doc; the per-line columns opened once per search), so its
first batch no longer waits for the whole search (cross-line rows: best
of 3 runs). "text": the exact total needed every candidate's text located
(locate time measured on a 2000-text sample and extrapolated, issue #21
probe) plus fetching all those texts from seforim.db. The in-order query's
78,061 hits now equal the in-order proximity's, as they should. Memory: no
per-hit arena; the peak moves by a few MB (a chunk's positions) and drops
for the 2M-hit word.

## Parity with FtsLib (measured)

`tools/parity/` runs KitveiHakodesh Core's own searcher on the installed FtsLib
index (`FtsLibDump`) and our demo (`SeforimSearch.Demo dump`) on the same DB
and query list, then diffs every hit with `compare.py`. The diff covers hit
order, word distance, snippet window, flags (short bit included), and each mark's
offsets and term.

On the full Otzaria DB (7,080,797 lines), first 2,000 hits per query: 16 of
17 queries were identical. That covers plain words, multi-word, `%`, `*`, `?`,
`|`, and distance 0/2/5. The one difference, `word~N`, was analysed: it was
OUR bug, not FtsLib's. The fuzzy DFA counted the intra-word quote of יצי"ק
as an edit, while every other query kind treats the quote as transparent
under ignore-gershayim. Fixed (fuzzy `ignore_quotes`); see the comparison
report for the current numbers.

The larger set (`tools/parity/queries-large.txt`, 58 queries: every query
kind, quoted words, distances, OR groups, long phrases), first 20,000 hits
per query, gave 48/58 identical on both index flavours. Every difference was
analysed before fixing (whose bug is it?):
- **FtsLib bug, fixed in FtsLib:** `HtmlWordScanner` computed a word's
  visible start without its quotes, which shifted long snippets' context trim
  (5 queries, 1-3 hits each). Fixed in KitveiHakodesh `e2a454ed`.
- **Our bug, fixed:** under the 2,000-term cap a word and its quoted
  variants (שב"ת, מל"ך) took separate slots, crowding rarer words out
  (`*דוש*`). They now share one slot (tantivy `e1ca3da4`, FM path too).
- **Our bug, fixed:** our `%` prefix table had `ום` (vav + final mem) where
  FtsLib's bytes are `ומ`: the port copied FtsLib's comment, not its escape
  (4 `%` queries). FtsLib's misleading comment fixed too (`2289253d`). Both
  tables are now checked byte-for-byte against FtsLib's escapes.

After the fixes those 10 queries are identical on both flavours; the full
58-query rerun is in the comparison report.

Full build (C# demo, .NET 10, 7,080,797 lines, measured 2026-10-21, index
format `tantivy7`: Otzaria's tokenizer rules):

| Stage | With positions | Without |
|---|---|---|
| indexing (read DB, tokenize, write segments, commits) | 10.7 min | 7.0 min |
| merge into one segment | 3.3 min | 2.1 min |
| FM-index sidecar | 0.1 min | 0.1 min |
| total | 14.2 min | 9.3 min |

The previous build (`tantivy6`) took 8.1 / 5.2 min on a quieter machine; the
merge, which does no tokenizing, also took 1.3x / 2x longer this time, so
most of the difference is the machine. The scanner itself, benchmarked
side by side on the same synthetic text, runs at the old speed (114-141
MB/s against 127-168 MB/s).

Index size 1,624.0 MB with positions (was 1,619.9): postings `.idx` 749.9
MB, positions `.pos` 720.2 MB, FM sidecar 43.9 MB, term dictionary 41.4 MB,
empty doc store 28.5 MB, fast fields (line id, position, book id) 25.8 MB,
field norms 14.2 MB. Without positions: 731.6 MB (was 723.7; `.idx` 581.1
MB, `.term` 37.7 MB, `.fmi` 43.9 MB). The dictionary now holds about 280
distinct chars (every script is a word char), so the FM sidecar uses 16-bit
codes. FtsLib's own index (`fts8`, same words): 746.0 MB (was 745.2 MB),
built in 16.2 min (lines 13.8 min, compact 1.0 min, term dictionary 1.4
min). Parity against the new FtsLib, both flavours: queries-diff10 10/10,
queries-large 61/61, queries-words 14/14, queries-rules 21/21 (period,
brackets and bidi chars inside words, Latin and Greek words, trailing
geresh, typographic quotes, presentation forms, paired readings, a word too
long to be indexed).

Search (same index, `search_report.py`, 17 parity queries): the engine's
part (walks + collecting every hit) takes 29–330 ms per query, and the first
batch of located results reaches the host in 0.4–0.9 s. The C# demo's
`search` command then fetches and locates EVERY hit, so its totals (0.7 s for
315 hits up to 50 s for 270k hits) measure the host's full-result loop, not
what a page needs.

## Differences from FtsLib (known, deliberate)
Deviations from Otzaria are named inline with each feature ("Deviations,
named"); the hit-count comparison in FTS-SPECS-AND-COMPARISONS.md classifies
every differing query.
- **Word-tightest window on an index with positions** (issue #21, the
  user's decision): the snippet window (and the occurrence a distance
  filter reads) is the one spanning the fewest word positions, the earliest
  on a tie, decided from the postings during the search; FtsLib picks the
  fewest UTF-16 units. 0.1-3% of the hits show another occurrence than
  FtsLib (measured on full-library samples; a single word: the first
  occurrence instead of the shortest), and in rare lines a distance filter
  passes under the word rule only. In order, the slots also follow in
  strictly increasing positions (a paired reading's two readings share one
  position and never chain), and in any-order proximity one position fills
  one group (both readings of a paired reading never fill two groups, as the
  span query on positions). An index without positions is FtsLib's rule
  exactly.
- **A quoted phrase across lines:** one phrase holding the whole query may
  cross into the next line here (Otzaria's literal phrase search); FtsLib
  has no cross-line matching, so it finds the in-line phrases only.
- **Quotes:** the index keeps intra-word quotes (רש"י) where FtsLib dropped
  them. Matching ignores them through the "ignore gershayim" option, which the
  FtsLib facade turns on by default, so results match FtsLib. After the
  last letter a geresh is kept (`'`, Otzaria), a gershayim never. Walks also
  match a term by its quote-free form unless `exact_quotes` is set (Otzaria's
  `QuoteTransparent`); in FtsLib mode that is FtsLib's own behaviour, since
  its index stored only quote-free terms (parity queries רמבם, רשי, זל).
- **Mark spans can contain tags.** A word that ends at a block tag (heading,
  `<br>`, …) has its mark run up to that tag's `>`, e.g. `בראשית</h1`. This is
  FtsLib's span exactly: its scanner flushes the word at the `>`. Renderers
  must strip tags inside a mark, as the KitveiHakodesh page does.
- **`%word`** uses FtsLib's GrammarExpander tables (34 prefixes / 32
  suffixes). The Otzaria prefix/suffix options are separate walk options.
- **Ketiv/qere expansion** (`expandKetiv`) is replaced by the Otzaria walk
  options (`ktiv_male_haser`). As FtsLib since KitveiHakodesh `71162c83`,
  ketiv also varies a wildcard's literal core (only leading / trailing
  `*`, no `?`) and a `%` word: each variant is expanded like the typed
  pattern and ORed in its slot. The rules are a per-word option,
  `ktiv_rules`: `otzaria` (default, Otzaria's vav / yod rules) or `ftslib`,
  a step-for-step port of FtsLib's `KetivExpander` (`hebrew-fts/src/ftslib_ketiv.rs`:
  preserved suffixes, edge vav / yod consonantal, single deletions, bare
  skeleton, base-3 insertion masks over the first 4 gaps, original-stem
  insertions, cap 40 distinct forms in FtsLib's order), whose forms are the
  walk's core forms. The FtsLib facade (C# demo) sets `ftslib`, so with ketiv
  on its plain, wildcard and `%` words take exactly FtsLib's variants; a test
  checks the port against output generated by running FtsLib's own code.
  Parity set: `tools/parity/queries-ketiv.txt` with `--ketiv` on both dumpers.
- **Order and distance:** results are in catalog order, as FtsLib (the
  default; Otzaria's relevance and generation orders are options). Word
  distance is checked from the line text, so an index without positions works.
- **Doc order = catalog order** (index format `tantivy11`): the index is
  created sorted by the `position` fast field (tantivy's `sort_by_field`,
  recorded in `meta.json`), so every segment is written in position order
  and the merge keeps it; the merged single segment's doc ids ARE the
  catalog order. `LineIndex::catalog_ordered` is true for such an index with
  one segment, and the search then skips its sort of all hits (catalog
  order, no cross-line hits). An update (delete + add) adds a segment: the
  flag is off and the sort runs until the next `merge_all`; deletes alone
  keep it. Older indexes (no sort setting) always sort. Build cost on a
  300k-line fixture (4 threads): indexing +50-60 %, merge +40-55 %; the
  search over 300k hits 53 ms -> 23-31 ms.
- **Hosts keep seforim.db open:** the C# demo rents corpora from a
  `SeforimDbFtsCorpusPool` (one connection each, with its schema probe, zstd
  dictionary and prepared statements; one user at a time, a second one
  opened for a concurrent search; up to 4 kept
  idle). Each rent is a fresh lease: Dispose returns the connection exactly
  once (a second Dispose does nothing, a disposed lease refuses calls), and a
  lease that ends with a result sequence still open (an abandoned
  enumerator) closes its connection instead of pooling it. Tested by the
  demo's `selftest` command (double Dispose, abandoned enumerator,
  concurrent rent, reopen on file change, and an end-to-end tantivy11 build
  + search on a small seforim.db-shaped fixture; net10 and net48); the Dart demo's `SeforimDb` keeps its one connection and prepared
  statements. Both reopen when the database file changes size
  or write time, or when `PRAGMA data_version` moved (a commit through the
  WAL, e.g. a new zstd dictionary; issue #8). The Dart demo opens the new
  connection before closing the old one, closes idempotently, and reads a
  book in chunks so no cursor outlives a call. Content decoding (C#, Core,
  Dart): a frame naming a dictionary the codec lacks re-reads `zstd_dict`
  once and retries; a `KLUv` string is decoded only when it is valid base64
  with a zstd frame header, and a frame that cannot be decoded (or, in Dart,
  malformed UTF-8) raises an error naming the line id instead of returning
  raw text.

## Open
- Quoted phrases: a whole-query phrase may cross a line in our engine but
  not in FtsLib (no cross-line there), and the C# FtsLib facade locates the
  line alone, so it keeps FtsLib's per-line hits. The negative query reads
  quotes as plain words. Parity sets hold no quoted query yet.
- Otzaria gaps found by the Dart demo (see `dart/README.md`):
  - ראשי תיבות and תרגום ארמי are done host-side in the Dart demo
    (Otzaria's dictionary assets → phrase / word alternatives).
  - Grouping (sameSection cards, identicalText merging) is done host-side
    in the Dart demo, with sections read from SQLite (`line_toc`) by the
    hits' line ids. The sameSection scope and its exclusion are the
    engine's (section ids in the index).
  - Category (facet) counts, Otzaria's search navigation tree, a one-snapshot
    result page (counts + category counts + one page grouped by book then
    section) and the count status label (exact / partial / truncated) are
    host-side in the Dart demo (`lib/facets.dart`, issue #11): the engine's
    per-book counts summed up seforim.db's category tree; no engine change.
- Full-DB rerun pending for index format `tantivy16` / FtsLib `fts12`
  (unterminated `<`, cross-line content edges, fuzzy swap = one edit; doc
  order = catalog order; transparent vocalized `<sup>`; base text = `isBaseBook` flag only, issue #6; footnotes as notes books with 64-bit ids, issue #13): the unit tests pass; the rebuilds, parity sets,
  the Otzaria hit-count comparison and the first-page profile have not been
  rerun.
- First-page latency (profiled, see FTS-SPECS-AND-COMPARISONS.md "First-page
  latency"): ours 350-420 ms vs Otzaria 55-110 ms for a common word. Causes:
  the C# host opened the SQLite corpus per search and paid cold first-use
  costs; the engine collects and sorted ALL hits before the first page.
  Built (`tantivy11`, full-DB numbers pending): the hosts keep seforim.db
  open, and hits arrive sorted (see "Doc order = catalog order"). Built
  (issue #21): the search streams on an index with positions, the first
  batch in 7-28 ms engine time on the full library (every hit still kept).
- Vocalized matching (nikud / te'amim) needs an index built with
  `keep_marks`; matching marks on a plain index is not built.
- Choosing span query vs post-filter for very broad expansions (needs a
  benchmark).
