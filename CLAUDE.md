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
  (the zero-width quote is passed in by hebrew-fts via `ZeroWidthBytes`); both
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
- Word lengths: only 2..29 LETTERS are indexed; quotes and marks don't count.
- FtsLib wildcard budgets: anchor ≥2, ≤4 `?`, leading `*` ≤3, trailing/internal ≤4,
  both ≤7. Cap 2000, shortest first, then doc count.

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
- Index MUST be merged to one segment after a build: every walk repeats per segment
  (*יצח* 4.8 s on 37 segments → 1.1 s on 1), and the FM-index sidecar needs one segment.
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
- FtsLib's fuzzy is plain Levenshtein (`Levenshtein.Distance`, no transposition); the
  DFA must be built with `transposition = false`.
- `%` is FtsLib's GrammarExpander (its own 34/32 tables), NOT the Otzaria walk options.
- Parity is checked with `tools/parity` (FtsLibDump + `SeforimSearch.Demo dump` +
  compare.py) against the installed FtsLib index at
  `%LOCALAPPDATA%\KleiKodesh\KitveiHakodeshService\FtsIndex`. Rerun it after any
  tokenizer / walk / snippet change.
- Bump `TantivySeforimIndex.IndexFormatVersion` whenever indexing output changes, or
  BuildLines keeps the stale index.
- PowerShell 5.1 strips quotes from JSON arguments to native exes: pass via a file.

## Open decisions / next

- Benchmark span query vs FtsLib-style post-filter for very broad expansions.
- Profile the C# search path (1.0 s net10 / 1.5 s net48 vs 0.38 s pure Rust).
- Otzaria gaps still open: grouping (sameSection cards, identicalText merging),
  relevance ordering.
  Otzaria's sources: `github.com/Otzaria/otzaria` (Dart) and
  `github.com/Otzaria/otzaria_search_engine` v5.0.0 (Rust `hebrew_query.rs`,
  `api/search_engine.rs`, `lexicons.rs`); clone them into the scratchpad to port.
- Done: Dart binding; full-DB side-by-side parity (16/17); FM sidecar; wordMatchMode;
  negative query (`ExcludeSpec`); תרגום ארמי as a HOST lexicon (Dart demo
  `translation_lexicon.dart`) and ראשי תיבות (host `acronym_lexicon.dart` + engine
  `phrase_alternatives`): app assets/dictionaries stay out of the engine.
- Trap: Git Bash / PowerShell mangle an ASCII `"` in args to Windows exes (רמב"ם became
  3 words). Test acronyms with the Hebrew ״, which the sanitizer folds to `"`.
- Otzaria's Acronyms.json has no רמב"ם entry (it has רמב"ן etc.); check the asset
  before assuming the code is wrong.
