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
   - find the tightest window covering every query word
   - add ±8 words of context and the ellipsis flags
   - mark every matched word inside the window
   - apply the word-distance and word-order filters
   - report the window's word count, so the host can tell a short window
     and locate again over the line joined with its neighbours (what
     counts as a neighbour is seforim.db knowledge, so that stays in the
     host: FtsLib's `SnippetDescriptorPipeline`, ported in the C# demo)

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
| `seforim-engine/` | The engine. **No database code**: the host feeds lines and texts. Includes the FM-index sidecar (FtsLib's `FmIndex`) for leading-`*` and fuzzy. | hebrew-fts |
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
   - *Window mode* (`set_window(Some(max_span))`): every slot at its own
     position, in any order, all inside one window of `max_span` positions
     (Otzaria's any-order proximity). One sweep over the slots' merged
     positions; a bipartite slot/position matching runs only when one
     position can fill several slots (a repeated word, overlapping walks).
   - The ordered check is a linear forward sweep per slot (every position
     where a valid chain can end), no backtracking.

Also small: `get_match_term_infos` honours the cap and the sink, and
`RegexPhraseWeight` is reachable crate-internally so SpanQuery can reuse its
positioned union. `PhraseScorer` is public (its constructor already was), so
the relevance order scores a phrase with tantivy's own BM25 over a union of
the matched terms per position. `FuzzyTermQuery`'s Debug text is unchanged (the new fields
are not printed), so upstream's query-parser tests stay as they are.

### hebrew-fts
- **Tokenizer.** A port of FtsLib's `HtmlWordScanner`:
  - HTML-aware: inline tags are transparent; block tags, entities and maqaf
    split words.
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
    them.
- **Walks:**
  - **Grammar** (`%word`, `word%`): 34 prefixes and 32 suffixes.
  - **Otzaria options:** grammatical/Aramaic prefixes and suffixes, ktiv
    male/haser, part-of-word, advanced prefixes/suffixes, ignore gershayim,
    typos (שגיאות כתיב), ignore nikud, ignore te'amim. Ported from Otzaria's
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
  - Deviations, named: the cap is ours (2000 per segment, shortest first,
    like every walk) where Otzaria takes up to 10000 terms (a sequence
    8192 per word) in dictionary order under postings budgets, and reports
    `truncated`; we report no truncation flag. Narrowing to Otzaria's term
    alphabet means a letter outside it, literal or class (an accented
    Latin letter, Cyrillic, Greek), never matches (as Otzaria). A byte
    class (`(?-u:...)`) is invalid, as Otzaria. The low-level request's
    `{"kind":"regex","pattern":...}` takes a pattern as is (no folding, no
    case folding, no 300-char limit; still whole-term and size-bounded). Otzaria's sequence may also continue onto the next line: that
    part is deferred with cross-line matching. An index that keeps marks
    is refused (its terms are vocalized; the folded pattern cannot match
    them): an error.
  - Not yet: Otzaria's `Line` kind (`CrossSpace`: an explicit space class
    such as `[ ,]`, inner anchors or `\b`, a wildcard-only word in a
    sequence), which Otzaria matches on each line's text. Such a query
    returns an error for now.
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
- **Index:** create or open; positions optional; add/delete/commit lines
  (`line_id`, `position`, `book_id`, text indexed but not stored; see the
  bare-minimum rule above) with a host payload (resume watermark,
  provenance stamp) committed atomically with the documents.
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
  - Otherwise the host feeds the hits' texts in hit order (any batch
    size); the engine validates each batch across all cores with exactly
    the snippet locator's test (no snippet, no term table) and adds the
    passing lines. `exact` turns true when every hit is counted. This is
    the no-positions flavour's path, and FtsLib's text-based distance on
    either flavour.
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
- **Search:** every matching line in catalog order (or by relevance /
  generation, see the result order item), plus a snippet session.
  - The session locates snippets for a whole batch across all cores.
  - It applies the word-distance and order filters. Widening a short
    window is the host's job: join the line with its neighbours and locate
    that text (no neighbour or DB logic in the engine).
  - It keeps the term table.
- **Search API: one `SearchOptions` object** (Rust `LineIndex::search_options`,
  C ABI `se_search` with a JSON that has `"query"`, C# `SeforimIndex.Search(SearchOptions)`,
  Dart `SeforimIndex.search(SearchOptions)`; same field names in each
  language's casing). Callers never build walks or slot kinds: the engine
  parses the query and builds one walk per word with all its options.
  - `query`: FtsLib syntax (`*` `?` `%` `~N` `|` stay in the string).
  - Word options, for every word: `grammar_prefixes`, `grammar_suffixes`,
    `aramaic_prefixes`, `aramaic_suffixes`, `ktiv_male_haser`,
    `part_of_word`, `advanced_prefixes`, `advanced_suffixes`,
    `ignore_quotes`, `exact_quotes`, `spelling_errors`, `ignore_nikud`,
    `ignore_teamim`, `typo_mode`, `prefix_max_letters`,
    `suffix_max_letters`, `part_of_word_max_letters`,
    `grammar_prefix_max_letters`. Wildcard, fuzzy and `%` words take only
    `ignore_quotes` (as FtsLib's parser).
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
    `wordOptions` / `alternativeWords`.
  - Query level: `max_word_distance`, `require_ordered`, `min_groups`,
    `word_match` (`{"mode":"all"|"any_word"|"most_words"}` /
    `{"mode":"at_least","count":N}`: Otzaria's wordMatchMode, the engine
    counts the slots it searches, a dropped word not counted; wins over
    `min_groups`), `context_words`, `proximity` (per TYPED word, see the
    proximity item), `exclude` (`{query, words, phrase_alternatives,
    max_word_distance, require_ordered, proximity}`; options are per
    word: a negative word is a word like any other, the global options
    are its default and `exclude.words` its own overrides and
    alternatives, same typed-word index rule as `words`; no separate
    global set for the negative query),
    `books`, `phrase_alternatives` (`[{query, ignore_quotes}]`), `order`
    (`"catalog"` default, `"relevance"`, `"generation"`) and
    `book_generations` (`[[book_id, generation_order], ...]`), see the
    result order item.
  - JSON: `{"query":"משה תורה","grammar_prefixes":true,
    "words":{"1":{"grammar_prefixes":false,"alternatives":["אורייתא"]}}}`.
  - The low-level `SearchRequest` (slots built by the caller) stays, for
    tests and special hosts; `SearchOptions` lowers to it.
- **Query groups** (the low-level slots): word (with options), wildcard,
  grammar (`%`), fuzzy (distance 1–3, FtsLib's range), and OR of
  alternatives.
- **Fuzzy** is hebrew-fts's own `HebrewFuzzyQuery` (the Levenshtein DFA
  tantivy uses, built here because tantivy's wrapper cannot be built from
  outside): plain Levenshtein edits (a transposition is two, as FtsLib's
  `Levenshtein.Distance`), cap 2000 per segment, closest edit distance first,
  then shortest with the quote zero width, then most frequent. The plain
  query and the span slot are the same weight.
  - **`ignore_quotes`** (on by default in the FtsLib facade, like every other
    group kind): distances are measured on the quote-free forms, so the
    indexed יצי"ק is one edit from יצחק, as FtsLib's quote-free term יציק is.
    With the FM sidecar the candidates are confirmed with the plain
    Levenshtein distance on the quote-free forms (FtsLib's
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
    it back. A `keep_marks` index (terms with Hebrew marks) gets no
    sidecar; it keeps using the walk. Symbol codes are 16-bit and sort keys
    take the width the alphabet needs (every script being a word char, the
    dictionary holds about 280 distinct chars), as in FtsLib's `FmIndex`.
  - Measured on the full index (1,755,598 terms): built in 2.4 s, 41 MB;
    `*יצח*` 41 ms against 1.1 s as a walk (FtsLib: 0.26 s).
- **Partial matching** (Otzaria's wordMatchMode): `min_groups` in the
  request (C# `MinGroups`, Dart `minGroups`, demo `--match any|most|atleast`).
  A line needs only that many of the groups (anyWord 1, mostWords n/2+1,
  atLeast N, clamped to 1..n); it runs as a BooleanQuery with a minimum
  number of matching clauses, and the snippet is the tightest window
  holding that many groups. As in Otzaria, distance, order and spacing are
  not applied in that case.
- **Exclusion** (Otzaria's negative query): `exclude` in the request (C#
  `SearchRequest.Exclude`, Dart `exclude`, demo `--exclude "words"
  [--exclude-scope distance|paragraph]`): the words to drop, as a query of their own
  with its own distance, order and spacing, every word required (Otzaria's
  `apply_advanced_negative_query`, which builds it with the positive's
  builder). Options are PER WORD, as for the positive words: the global
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
  exclusion (Otzaria's sameSection / `scoped_words_query`) is host-side,
  since the index holds no sections: one search per negative word, its
  line ids mapped to sections with SQLite (`line_toc`), the sets
  intersected, and the positive hits in those sections dropped (a hit
  without a section is never dropped). The Dart demo does this for
  `--exclude-scope section` (its per-word searches still use the global
  options: section scope is on hold).
- **Phrase alternatives** (for Otzaria's ראשי תיבות): `phrase_alternatives`
  in the request (C# `PhraseAlternatives`, Dart `phraseAlternatives`). Each
  is a phrase ORed with the main query: its words adjacent, in order and all
  required (a span query with positions; without positions an AND checked on
  the text with FtsLib's distance rule, which lets a repeated word fill a
  gap). `locate` tries the main query, then each phrase, and marks the words
  of whichever matched. The acronym dictionary itself stays in the host.
- **Otzaria word proximity** (`proximity` in the request, `ProximitySpec`;
  C# `SearchRequest.Proximity` = `Proximity.Ordered(gaps)` /
  `Proximity.AnyOrder(distance)`, Dart `Proximity.ordered` / `.anyOrder`;
  also on `ExcludeSpec` for the negative query). Otzaria's rule
  (`gap_phrase.rs` `Proximity`, `hebrew_query.rs` `resolve_proximity`):
  - `ordered { gaps }`: words in query order, at most `gaps[i]` words
    between word i and i+1 (Otzaria: `distance` for every pair, or
    customSpacing per pair).
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
  C# `SearchOptions.Order` / `BookGenerations`, Dart `order` /
  `bookGenerations`, Otzaria demo `--order catalog|relevance`;
  `seforim-engine/src/relevance.rs`). Catalog order stays the default and
  costs nothing: no score, no doc id kept.
  - `generation`: Otzaria's `OrderKind::Generation`: books by the
    generation order the host gives per book (Otzaria's app pushes its
    `generationOrder`; the engine keeps no generation), then catalog order.
    A book without one comes last.
  - `relevance`: Otzaria's `order_by_score()` over the query Otzaria builds
    for the same search, computed after the search for the hits only, from
    the terms the walks matched (sinks); ties in catalog order (Otzaria:
    ascending doc address). Per shape:
    - one slot: Otzaria's tiers (`match_tier`, ported verbatim: the typed
      form or an alternative 0, inside a longer word 1, a ktiv male/haser
      variant 2, the rest 3), a line scores the sum of 1000 / 100 / 10 / 1
      over the tiers it holds. A keep-marks index is Otzaria's vocalized
      field, which it never tiers: 1 per line.
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
    adjacency need positions, so on an index without positions those shapes
    return an error (adding only frequencies would not serve them: about
    +170 MB, the `.idx` difference of the two full builds).
  - Deviations (named): the phrase IDF is taken from the slots' typed words
    (Otzaria's from its regex pattern strings, which equal the word for a
    plain word): a constant factor per query, the order inside a query is
    the same. Otzaria's flat-scored degrade path (phrases whose expansions
    pass its ceiling) is not ported: our walks cap instead of failing, so
    every phrase is scored. Otzaria's fuzzy mode with its magic dictionary
    (`build_lexical_fuzzy_query`, the lexical tier 30) is not ported: the
    engine has no lexical forms; ours is its plain builder. On a keep-marks
    index fuzzy words score 1 (Otzaria's vocalized fuzzy is unranked), and a
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
  `SearchResult`, `SnippetDescriptor` (the query string goes to the engine
  as `SearchOptions`; FtsLib's `QueryParser` is ported in hebrew-fts), and
  `TantivySeforimIndex` (`Search`, `LocateSnippetWindow`,
  `LocateSnippetWindowOverNeighbourLines`, resumable `BuildLines`,
  `GetResumeState`).

## Build and search reports

- `SeforimSearch.Demo build` prints progress, then each stage's time
  (indexing, merge into one segment, FM-index sidecar), the total, and the
  index size in total and per file type.
- `python -I tools/report/search_report.py <demo exe> <index> <queries> <out> [--runs N]`
  runs every query with the engine's stage timing on and writes
  `search-report.md`: per query the engine hits and the time of each stage
  (query weight; FST walks / FM lookups + hit collection; host text fetch +
  snippet locate + widening), time to the first batch, total, and exit code. Every run's full
  log is kept next to the report. `--runs N` adds one warm-up run, then
  reports each stage as the median of N runs (every run in `search-<n>.log`).

## Snippet-locate speed

The host loop that reads every hit spends most of its engine time locating
snippets. Measured single-thread on 2.2 KB HTML lines
(`cargo run --release -p hebrew-fts --example locate_bench`): the scanner
went from ~100 to ~380 MB/s and the full locate from ~70 to ~250 MB/s, with
byte-identical output (the old scanner is kept as a test-only reference and
compared on ~65 edge cases plus 20,000 random strings). `locate_many` splits
work by bytes across reused per-thread locators. An 802,736-hit search read
in full went from 54.8 s to 29.0 s (C# demo, no-positions index).

## Parity with FtsLib (measured)

`tools/parity/` runs KitveiHakodesh Core's own searcher on the installed FtsLib
index (`FtsLibDump`) and our demo (`SeforimSearch.Demo dump`) on the same DB
and query list, then diffs every hit with `compare.py`. The diff covers hit
order, word distance, snippet window, snippet lines, flags, and each mark's
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
  options.
- **Order and distance:** results are in catalog order, as FtsLib (the
  default; Otzaria's relevance and generation orders are options). Word
  distance is checked from the line text, so an index without positions works.

## Open
- Otzaria's user-regex `Line` kind (a pattern matched on each line's text:
  an explicit space class, inner anchors or `\b`, a wildcard-only word in
  a sequence) returns an error: its single-line part needs an engine
  design (candidate lines from the pattern's required literals, the host's
  texts validated on Otzaria's line view).
- Otzaria gaps found by the Dart demo (see `dart/README.md`):
  - ראשי תיבות and תרגום ארמי are done host-side in the Dart demo
    (Otzaria's dictionary assets → phrase / word alternatives).
  - Grouping (sameSection cards, identicalText merging) and sameSection
    exclusion are done host-side in the Dart demo, with sections read from
    SQLite (`line_toc`) by the hits' line ids.
  - The positive query's sameSection scope (words spread over a section's
    lines) is missing.
- A full build compared side by side with FtsLib on the same queries.
- Profiling: C# search is ~1.0 s (.NET 10) / ~1.5 s (net48), against 378 ms in
  the pure-Rust smoke test for the same query. That gap includes process start
  and index open; not profiled yet.
- Choosing span query vs post-filter for very broad expansions (needs a
  benchmark).
