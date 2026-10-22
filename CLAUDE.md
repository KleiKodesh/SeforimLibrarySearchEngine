# SeforimdbSerachEngine

Hebrew full-text search over the Otzaria / Zayit `seforim.db`, built on a modified
tantivy, returning **exactly** what KitveiHakodesh's FtsLib returns, plus Otzaria's
search options. Read `README.md` for the architecture; this file is the working
knowledge: rules the user set, how the parts fit, and lessons learned the hard way.

**Purpose:** a new and improved search engine for Otzaria, and one engine that unifies
the search of the KleiKodesh project (KitveiHakodesh, today on FtsLib) with Otzaria,
so both apps share the same engine, index format and results.

**The two main changes** (the reason this engine exists; keep them central):
1. **Custom FST walks**: every search option is an automaton walked over the term
   dictionary, shortest first, capped inside the walk.
2. **Matched FST terms returned with the search** (`MatchedTermsSink`): the hits plus
   the words the walks matched, in one pass, with no separate query.

**Keep this file learning.** When a session teaches something non-obvious (a rule the
user states, a trap hit, a decision made and why), add it to the right section below
in one or two lines. Remove entries that turn out to be wrong.

## The user's rules (stated explicitly, follow them)

- **THE INDEX IS THE BARE MINIMUM (engine + seforim.db in tandem).** The engine is
  always used together with the seforim.db SQLite DB, so the index stores only the
  postings (+freqs/positions when built with positions, for span/slop) plus
  `line_id`, `position` and `book_id` (book_id only because the engine must support
  FILTERED SEARCH by book), plus (user's decision 2026-10-21, for cross-line and
  sameSection) per-line `word_count`, `line_flags` (no-cross bit) and `section_id`. No text, nothing the host can get from SQLite by line id.
  Grouping, sections, line index, references, neighbours: the HOST does them with
  SQLite on the returned line ids ("easy sqlite super speed"). Applies to C# and Dart
  too, and to any similar future need.
- **NO HACKS. A missing capability is built properly INTO THE ENGINE.** If Otzaria (or
  FtsLib) has a feature we lack, design it as a real engine feature, never a host-side
  workaround or a composition of existing calls that only approximates it. Example (the
  user's): Otzaria's per-pair word distance must be an engine capability, evaluated with
  positions on an index that has them (SpanQuery gaps) and validated on the line text by
  the locator on an index without positions — NOT "search by pairs" or remapping it onto
  a different distance rule. Same for every other gap (cross-line phrases, sameSection
  scope, quote-free matching, ...).
- **Quick tests only by default** (user's rule): unit tests, otzaria_cases, small fixtures,
  builds. Full-DB rebuilds, full parity sets, speed benchmarks and hit-count reruns run ONLY
  when the user explicitly asks; meanwhile mark the affected report sections as stale.
- **Long runs: progress + full log + error tracking, always.** Any build, index
  build, parity dump or benchmark that takes more than a few seconds writes its FULL
  output to a log file (`2>&1 | tee <scratchpad>/logs/<name>.log`), shows its progress
  while it runs, and records the exit code (`${PIPESTATUS[0]}`). Never pipe a long run
  through `tail`/`head`/`grep` alone: that hides progress and swallows errors (a failed
  index build was lost this way). On failure, read the log's error first and report it.
  Give the user a progress line (stage, count, ETA) when asked or when a run is long.
- **Time units in reports: ms below one second, minutes otherwise, never seconds**
  (e.g. 830 ms, 7.1 min). Search comparisons come with charts: each stage and the
  total, per query and per engine.
- **`FTS-SPECS-AND-COMPARISONS.md` (repo root) stays up to date** (user's rule): feature specs,
  index builds, FtsLib parity, speed comparison with charts (ours both flavours, FtsLib,
  Otzaria) and the Otzaria hit-count comparison. Regenerate it with
  `python -I -X utf8 tools/report/specs_report.py <scratchpad>` after every build, parity or
  speed run, and update its specs table with every feature change, in the same commit.
- **Specs are generated together:** the "Current specs" block here, README's Specs section
  and FTS-SPECS-AND-COMPARISONS.md all come from `tools/report/specs_report.py`; never
  hand-edit text between the `SPECS:BEGIN` / `SPECS:END` markers, rerun the script.
- **Every build and search run is reported in full.** An index build reports total
  time, the time of each stage (indexing, merge, FM sidecar) and the index size
  (total and per file type): the C# `build` command prints all of it. Search runs go
  through `tools/report/search_report.py`, which keeps every run's full log and writes
  `search-report.md`: per query engine hits, weight ms, walks+collect ms (the walks run
  inside scorer creation, not in the weight), host ms (text fetch
  + locate + widening), first batch, total, and exit code. Quote these numbers to the
  user; keep the logs and report in the scratchpad `logs/`.
- **Parity with FtsLib is the goal.** Port FtsLib/Core logic step for step ("tit for
  tat"); no generalizing, no guessing. Before porting, quote the C# and list its exact
  steps; the user checks. Deviations must be named and justified.
- **Port the CURRENT code.** KitveiHakodesh: `C:\Users\Public\Documents\KleiKodeshProject\KitveiHakodesh`.
  FtsLib: `CSharpBackend\FtsLib-Csharp\FtsLib`. Core: `CSharpBackend\KitveiHakodesh.Core`.
  Check on-disk vs git HEAD (`git status`) first; files were once deleted on disk.
- **No database code in Rust / tantivy.** Anything that needs SQLite or knows the
  seforim.db schema lives in the C# demo (`csharp/SeforimSearch.Demo`). The engine is
  fed lines and texts by the host. This includes widening short snippets over
  neighbour lines: "it has to do with the seforim db", so no join/re-locate helper in
  Rust; the host joins the texts and calls the plain locate (user's rule).
- **tantivy/ is our modified copy** ("KleiKodesh version", the user dislikes "fork"
  meaning anything online). Commit straight to its `main`, no branches. It is a git submodule
  (origin = KleiKodesh/tantivy); after tantivy commits, push it, then `git add tantivy` here.
- **Engine changes belong IN tantivy when they reuse what the engine already computes**
  (e.g. keep the FST walk's matched words instead of throwing them away). Must cost
  nothing when unused.
- **tantivy changes stay upstream-PR ready.** No Hebrew / FtsLib rule inside tantivy
  (the zero-width quote and marks are passed in by hebrew-fts via `ZeroWidthChars`); both
  dictionaries (FST and quickwit SSTable) must build and pass; don't touch upstream's
  tests (hide new query fields from Debug instead); rustfmt-clean; CHANGELOG entry.
  Test both: `cargo test --lib -- query:: termdict::` and the same with
  `--features quickwit`.
- **Expansion caps apply inside the walk**: walk shortest-first, stop at the cap, so the
  closest matches come first and the walk stays short. Applies to every wildcard /
  regex / walk expansion. Cap 2000 (FtsLib's), applied PER SEGMENT (user's
  decision; replaced the global cap).
- **Options are the user's choice**: nikud / te'amim ignore, quotes ignore, index with
  or without positions — offer them, never force one behaviour.
- **Options are PER WORD** (user's decision): a negative (exclude) word is a word like
  any other, with its own `WordOptions` and alternatives (`exclude.words`); the top-level
  SearchOptions fields are only the default for words that set nothing. No separate
  global option set for the negative query (Otzaria's negativeGlobalSearchOptions is
  expanded onto each negative word by the host).
- **C# must build and run on .NET 10 AND .NET Framework 4.8.** Package =
  netstandard2.0; demo = `net10.0;net48`, x64.
- **The C# demo mirrors FtsLib's API** (same class / member names) for easy switching.
- **Commit as you go**, one commit per finished step, without waiting to be asked;
  tests green first.
- **Commit only this session's own work.** Other sessions may be editing the tree:
  stage your files by explicit path (never `git add -A` / `.`), and check the staged
  diff holds only your changes before committing.
- **Run a QA subagent before EVERY commit**: it reviews the staged diff (correctness,
  FtsLib parity, README kept current, no AI co-author line). Fix what it finds, then commit.
- **NEVER add Claude as co-author** (no `Co-Authored-By: Claude` or any AI attribution
  line) in ANY commit or PR, in this repo AND in tantivy/. No exceptions, ever.
- **Keep README.md current automatically**: every commit that adds, changes or
  removes a feature, option, layout item, known difference or open item updates
  README.md in the SAME commit. Never leave it for later; don't wait to be asked.
- **Multi-platform**: any native OS through one C ABI; web goes through a server.
  The index is shipped prebuilt AND must be buildable/updatable on the user's machine.
- Answer short. Confirm understanding before big work when asked. Report honest
  numbers and failures.

<!-- SPECS:BEGIN (generated by tools/report/specs_report.py) -->
## Current specs (generated; update with the specs md)

Generated by `tools/report/specs_report.py` together with FTS-SPECS-AND-COMPARISONS.md and the README Specs section: rerun it whenever the engine, index format or a full-DB run changes. Index formats: ours `tantivy11`, FtsLib `fts9`. Features: FtsLib syntax + parity, per-word Otzaria options as FST walks (cap 2000 per segment), proximity / word match, vocalized walks, user regex (word, sequence, line), relevance / generation order, totals + per-book counts, cancellation, truncated flag, cross-line (enumerators skipped) and same-section, positions snippets, FM sidecar.
Builds: ours, positions 3.48 min 1,585.5 MB; ours, no positions 2.82 min 711.7 MB; ours, positions + keep marks 3.52 min 1,634.2 MB; FtsLib (fts9) 1.95 min 738.1 MB; Otzaria v5.0.0 1.42 min 1,886.0 MB.
Parity with FtsLib: all sets identical (diff10, large, words, rules, ketiv; both flavours).
Machine: OS mac (Darwin 27.0.0), arch: arm64; CPU Apple M4, cores 10, RAM 16 GB; disk free at out dir 151Gi.
All results current.

<!-- SPECS:END -->

## How the parts fit

```
tantivy/ (our copy)   MatchedTermsSink · shortest-first capped walk · SpanQuery
   ↑
hebrew-fts/           tokenizer (FtsLib scanner) · walks · wildcard · snippet locator
   ↑
seforim-engine/       line index fed by host · search in catalog order · snippet session
                      · FM-index sidecar (FtsLib FmIndex) for leading-* and fuzzy
   ↑
engine-ffi/           C ABI → seforim_search_native.dll
   ↑
csharp/SeforimSearch.Engine/   netstandard2.0 P/Invoke package
   ↑
csharp/SeforimSearch.Demo/     seforim.db code (copied from Core) + FtsLib-shaped facade
```

Search flow: the query string goes through `QueryParser` (FtsLib rules) and becomes
groups (AND across, OR inside). Each group is a walk with a sink, and the
BooleanQuery returns every hit's line id + catalog position. The host fetches texts
in FtsLib's batch plan (256×16, then doubling to 2048). The engine then locates
each batch in one multi-core call and applies the distance and order filters. A
short window is the HOST's business: it joins the line with its same-book
neighbours (radius 2, non-empty lines; FtsLib's `SnippetDescriptorPipeline`, in the
C# demo) and locates that text. Rust holds no neighbour logic (user's rule: it is
seforim.db knowledge).

## Key facts about the data and FtsLib

- seforim.db shapes (probe, never trust `schema_meta`): Zayit (`line.content` TEXT),
  Otzaria pre-6, Otzaria 6+ (`line_content` sidecar, zstd with a dictionary from
  `zstd_dict`; match frames by the dict id INSIDE the dict bytes, e.g. 32768; some rows
  are base64 text of a frame, prefix "KLUv"). Decide by the value's runtime type.
- DB path order: registry `HKCU\Software\VB and VBA Program Settings\KitveiHakodesh\Database`,
  `%APPDATA%\otzaria\books\seforim.db`, `%APPDATA%\io.github.kdroidfilter.seforimapp\databases\seforim.db`.
- Catalog order = `SeforimDbBookTreeOrder` (per folder: base texts, subfolders, rest;
  negative orderIndex last) then `lineIndex, id`. FtsLib's FTS rowid IS this position.
- `line_toc` is the authoritative TOC mapping (`line.tocEntryId` is mostly NULL).
- FtsLib indexes with `detail=none` (no positions); distance is computed from the
  text by the locator. Snippet = ONE best window per hit (tightest span, strict-< tie
  = earliest), marks = every query-term token in the window.
- Offsets are UTF-16 (.NET/JS indices) everywhere the page reads them.
- Word model (user: "fix fts lib, make it match otzaria", 2026-10-21): FtsLib's words =
  Otzaria's: runs of letters AND ASCII digits, any length, one letter included (`ב5`
  one word; digits script-neutral); a term over 128 UTF-8 bytes (inner quote = 1 byte)
  is not indexed / dropped from queries but keeps its position. FtsLib index `fts9`
  (ours `tantivy10`), our scanner = FtsLib's, so there is ONE model (the old `word_model` option is gone).
  A dropped one-letter word is a bug (a x b checks x).
- FM index codes are u16 (sigma up to 65536); suffix-sort keys pack codes at the
  alphabet's own bit width (`sort_key_bits` in `fm_index.rs`). On a keep_marks dictionary the FM text is the mark-free form.
- **keep_marks = Design 1 (user's decision):** terms keep their marks (canonical order),
  every walk reads them zero-width (`MarkFree`) unless match_nikud / match_teamim
  require typed marks; the cap groups mark-free forms. Vocalized mode filters to lines
  carrying a mark (`marked` field), as Otzaria's textVocalized. Old exact-marks mode gone.
- FtsLib wildcard budgets: anchor ≥2, ≤4 `?`, leading `*` ≤3, trailing/internal ≤4,
  both ≤7. Cap 2000, shortest first, then doc count.

## Benchmarks on any machine

`tools/bench/run-all.sh` runs everything (builds, indexes, parity, speed, Otzaria hit counts,
report); config `tools/bench/bench.env`. A request like "run the benchmarks" means following
`tools/bench/AGENT-RUNBOOK.md` end to end. The inventory step (`--steps detect`) comes first:
reuse the existing environment, DB, binaries, indexes of the current format and finished
results; never redo work. KitveiHakodesh is the submodule `external/KleiKodeshProject`
(pinned; bump it when FtsLib changes). Open items: the runbook's TODO list.

## Lessons learned (traps already hit)

- **PowerShell 5.1 text pipes corrupt Hebrew** (reads UTF-8 as ANSI). Never edit source
  with `Get-Content | Set-Content`; use the Edit/Write tools or `[IO.File]` with an
  explicit `UTF8Encoding($false)`.
- `cargo`/`rustc` are not on PATH in new shells: prefix
  `$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"`.
- First tantivy build ~10 min; fork tests: `cargo test --lib query::` (~30 s after).
  Workspace uses rust-lld + `debug = 0` for speed. Run long builds in the background.
- tantivy's `IndexWriter` needs ≥15 MB heap PER THREAD; cap threads by budget.
- zstd dictionary training needs enough sample data (tests pad samples).
- `MatchedTermsSink` keys a FuzzyTermQuery's Debug output; query-parser tests assert
  Debug strings, so hide new query fields from Debug; never edit upstream's tests.
- C# search sessions must outlive result enumeration (Core snippets the last batch
  after the loop); the native session is freed by its SafeHandle.
- The native last-error must be cleared at the start of every FFI call, or stale
  errors look like new failures.
- Core's sources use implicit usings; keep `ImplicitUsings` on in the demo.
- XML comments in .csproj cannot contain `--`.
- The cdylib must not share a crate name with a dependency (it is
  `seforim_search_native`).
- A quote AFTER the last letter (יצחק") is punctuation: FtsLib drops it. Keeping it made
  separate terms and broke parity everywhere. Only quotes BETWEEN letters stay.
- Doc order = catalog order: the index is created with tantivy's `sort_by_field` on
  `position`, so the merged segment needs no hit sort (`LineIndex::catalog_ordered`:
  sorted setting AND one segment). Never skip the sort on several segments (an update's
  extra segment) or with cross-line hits appended. Costs ~50 % more indexing time.
- C# corpus pool: `SeforimDbFtsCorpus` is a LEASE on a pooled `SeforimDbCorpusConnection`
  (idempotent Dispose; lazy sequences counted, an abandoned one closes the connection).
  Run `SeforimSearch.Demo selftest` (both TFMs) after touching it.
- Index MUST be merged to one segment after a build: every walk repeats per segment
  (*יצח* 4.8 s on 37 segments → 1.1 s on 1), and the FM-index sidecar needs one segment.
- **Deleted docs were hits.** Search collected through `weight.for_each_no_score`, which
  does NOT skip deleted docs (tantivy's collectors check `segment.alive_bitset()`
  themselves). An updated line (delete + add) came back in its old version too when its
  segment kept other live docs. It looked like a 1-in-9 test flake because a fully
  deleted segment is dropped whole. Fixed; `deleted_docs_are_not_hits` reproduces it
  deterministically (one indexing thread). Any new doc loop must check the alive bitset.
- **Port tables from FtsLib's CODE, never from its comments.** FtsLib writes Hebrew
  tables as `\u` escapes with the letters in a comment; one comment was wrong (`ום`
  for `\u05D5\u05DE` = `ומ`) and our port copied the comment, losing every ומ- form.
  Check any ported table byte-for-byte against the escapes
  (scratchpad `grammar_tables_check.py` did it for the grammar tables).
- **Fix FtsLib when FtsLib is wrong** (user: "just fix the fts lib"), in the
  KitveiHakodesh repo, committing only the touched file (that repo has other people's
  uncommitted work), no AI attribution.
- **A parity difference is analysed before it is fixed** (user's rule: is it our gap or
  a bug in FtsLib, or the reverse?). Example: `יצחק~1` differed because FtsLib's
  quote-free term יציק was 1 edit away while our DFA counted the quote in יצי"ק: our bug
  (fuzzy ignored ignore-gershayim), fixed with fuzzy `ignore_quotes`.
- Merge race: tantivy's default merge policy merges in the background during a build,
  so a segment list read earlier goes stale and `merge` fails with "segments that were
  merged could not be found in the SegmentManager" (two full builds died this way; the
  first error was lost to a `| tail`). `merge_all` now sets `NoMergePolicy`, reads ids
  from `index.searchable_segment_ids()`, and retries if an in-flight merge wins.
- **Leading-`*` and fuzzy go through the FM-index sidecar** (`seforim-engine/src/fm_index.rs`,
  a port of FtsLib's `FmIndex.cs`; `fm_sidecar.rs` builds `<segment>.fmi` after the merge;
  `fm_expand.rs` = FtsLib's `IsFmWildcard` / `ExpandWildcardWithFm` / `ExpandFuzzy`).
  The user's instruction: "build an fm index like in fts lib", not a trigram index. The
  sidecar only finds candidates (its text is quote-stripped); each is verified with the
  walk's own automaton and capped with the walk's order, so FM == walk (the
  `fm_sidecar_equals_the_walk` test proves it; the switch it uses,
  `test_only_disable_fm`, is `#[cfg(test)]` and must never reach production code).
  *יצח* 1.1 s → 41 ms.
- FM-index trap: the suffix order "up to the first stop, then by position" puts the
  SEP rows in position order, which is NOT LF order. Never map into the SEP range;
  a fuzzy match reads its term off the term-start row (BWT == SEP) instead. FtsLib
  used a rank→rowid table for the same reason.
- Fuzzy counts a swap of two adjacent letters as ONE edit (user's decision 2026-10-22,
  = Otzaria): DFA `transposition = true`, FtsLib `Levenshtein.Distance` / FM `FindFuzzy`
  and our FM verify are optimal string alignment. A q-gram filter must allow n + 1 grams
  lost per swap.
- An unterminated `<` (no `>` later in the line) is a plain separator in both scanners
  (Otzaria keeps it); it used to swallow the rest of the line.
- Cross-line joins CONTENT words (Otzaria `content_range`): line-edge enumerators are
  skipped via the `line_first` / `line_last` fields (`hebrew-fts/src/line_edges.rs`).
- `%` is FtsLib's GrammarExpander (its own 34/32 tables), NOT the Otzaria walk options.
- FtsLibDump off Windows uses `KitveiHakodesh.Core.Portable` (KitveiHakodesh repo: plain
  net10.0 project linking Core's own FTS / seforim.db / catalog sources; registry and kernel32
  calls behind `OperatingSystem.IsWindows()`). New Core code reached from the FTS path must keep
  that guard, or the Mac benchmark breaks. The demo's `PlatformTarget` is x64 on Windows only.
- Parity is checked with `tools/parity` (FtsLibDump + `SeforimSearch.Demo dump` +
  compare.py) against the installed FtsLib index at
  `%LOCALAPPDATA%\KleiKodesh\KitveiHakodeshService\FtsIndex`. Rerun it after any
  tokenizer / walk / snippet change.
- Bump `TantivySeforimIndex.IndexFormatVersion` whenever indexing output changes, or
  BuildLines keeps the stale index.
- The section of a line is `line_toc.tocEntryId` (covers ~all lines); `line.tocEntryId`
  is mostly NULL. Hosts read it from SQLite by line id (Dart `SeforimDb.sectionsOf`).
- Line index stays out of the index (host reads it from SQLite). Sections were
  first kept out too; the user later decided a per-line section id fast field so the
  sameSection scope (and its negative) runs in the engine in one pass.
- PowerShell 5.1 strips quotes from JSON arguments to native exes: pass via a file.
- Git Bash rewrites an argument that starts with `/` into a Windows path (MSYS path
  conversion): a regex query `/.../` silently became a path. Set `MSYS_NO_PATHCONV=1`
  (and then pass `C:/...` paths, not `/c/...`).
- Git Bash heredocs turn `\\u` into `\u`: Python with `\u` escapes must be written to a
  .py file (Write tool), never a heredoc. `python -I` ignores PYTHONIOENCODING: use
  `python -I -X utf8` for scripts that print Hebrew (compare.py).
- Tokenizer = Otzaria's rules in both engines (since tantivy7 / fts8): terms keep `'` and `"`
  (both zero-width in walks); a trailing geresh is in the term, so every automaton must
  accept a quote AFTER its pattern too (the wildcard missed it: 5 parity diffs).
- With every script a word char the dictionary has ~280 chars: FM codes are 16-bit.
  FtsLib's `UnicodeTables.cs` is generated from Rust (`tools/unicode-tables/gen.rs`).
- **NetFree filter blocks requests ("API Error: 418 I'm a Teapot")**, killing agents twice.
  Rule (user's): keep Hebrew content out of prompts, reports and tool output where possible;
  refer to Hebrew by unicode escapes / hex / file+line, never paste Hebrew text or long Hebrew
  dumps; do Hebrew-heavy work (reading Otzaria/FtsLib sources, test data) inside subagents
  told the same, and have them report without Hebrew.

## Open decisions / next

- **User's goal: every search feature Otzaria's current engine has that we lack gets
  added** (from the feature matrix of otzaria_search_engine v5.0.0): vocalized matching
  without a keep-marks index. (sameSection scope and cross-line: built, see above.) Semantic search:
  ask the user first.
- **Reuse Otzaria's tests**: its engine has ~309 inline-line search tests
  (`rust/src/api/search_engine.rs`) and ~92 query tests (`hebrew_query.rs`); plan in
  scratchpad `logs/otzaria-test-cases.md` (first 30 cases already translated to our
  API). Paired readings: done (engine rule; hosts call
  se_reduce_paired_readings). Suspected gap to verify: maqaf counted as two words for distance.
- **REMIND THE USER: matching across lines (`cross_line`, line-edge enumerators skipped
  as Otzaria) and the sameSection scope are BUILT and unit-tested, but NOT verified on
  the full DB.** Whenever the user asks whether everything is ready, say this explicitly.
- **Full-DB reruns pending for `tantivy11` / `fts9`**: both rebuilds, the parity sets,
  the Otzaria hit-count comparison (unterminated `<`, content edges, fuzzy swap) and the
  first-page profile (index sorted by position, hosts keep the DB open).
- **First-page latency** (FTS-SPECS "First-page latency"): ours 350-420 ms vs Otzaria
  55-110 ms. Causes: per-search SQLite corpus open + cold first-use costs in the C# host;
  full-hit collection + sort of all hits. Built: corpus kept open (C# pool, Dart kept
  connection), hits arrive sorted (index sorted by position). Still open: top-K first page.

- Benchmark span query vs FtsLib-style post-filter for very broad expansions.
- Relevance / generation order (`relevance.rs`) is scored AFTER the search, for the hits
  only, from the sinks' terms: the walks run inside scorer creation, so the terms exist
  only after collection. Otzaria's score depends on its query shape (tiers, phrase BM25,
  span bonus, fuzzy floors); phrases use tantivy's own `PhraseScorer` (made public).
  **Otzaria app checkout: `C:/Users/Public/Documents/otzaria`** (KleiKodesh/otzaria; pins
  otzaria_search_engine v5.0.0, same as the scratchpad copy): use it for parity checks.
  Otzaria's sources: `github.com/Otzaria/otzaria` (Dart) and
  `github.com/Otzaria/otzaria_search_engine` v5.0.0 (Rust `hebrew_query.rs`,
  `api/search_engine.rs`, `lexicons.rs`); clone them into the scratchpad to port.
- Done: result order option (catalog / Otzaria relevance / generation, `relevance.rs`);
  exact totals + per-book counts (`SearchResult::counts` / `count_batch`);
  cancellation (`CancelToken`, pull-based streaming); user regex `/.../` term kinds
  (Word, Sequence, Line); vocalized walks (keep_marks); ktiv_rules (otzaria / ftslib);
  truncated flag + relevance-needs-positions notice; positions snippets; Dart binding; full-DB side-by-side parity (16/17); FM sidecar; wordMatchMode;
  negative query (`ExcludeSpec`); תרגום ארמי as a HOST lexicon (Dart demo
  `translation_lexicon.dart`) and ראשי תיבות (host `acronym_lexicon.dart` + engine
  `phrase_alternatives`): app assets/dictionaries stay out of the engine.
- **Snippets from positions** (`positioned.rs`, `locate_lines`): the scan itself stays
  (FtsLib scores windows by character span and needs UTF-16/visible offsets), so the
  gain is bounded (15-24% on long lines, nothing on short ones); a cost rule picks the
  path per batch. Postings hold a term TWICE at one position when a paired reading
  repeats its word: dedup before matching words to positions.
- Trap: Git Bash / PowerShell mangle an ASCII `"` in args to Windows exes (רמב"ם became
  3 words). Test acronyms with the Hebrew ״, which the sanitizer folds to `"`.
- Otzaria's Acronyms.json has no רמב"ם entry (it has רמב"ן etc.); check the asset
  before assuming the code is wrong.
