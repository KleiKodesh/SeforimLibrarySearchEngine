# otzaria-driver

Headless driver for Otzaria's search engine (`search_engine` 5.0.0, tag v5.0.0, at
git dependency in `Cargo.toml`, unmodified) to benchmark it against SeforimdbSerachEngine on
the same `seforim.db`.

## Build

```
cargo build --release   # or: tools/bench/run-all.sh --steps build-tools
```

`Cargo.lock` is a copy of the Otzaria crate's lock, so dependency versions match the app.
The Otzaria crate's `[patch.crates-io]` (Otzaria/tantivy rev 8b88320, vendored tantivy-fst
with STATE_LIMIT 8192) only applies at a build root, so it is repeated in this Cargo.toml.
Features: `default-features = false, features = ["sqlite-bundled"]` (no semantic backend).

## Commands

```
target/release/otzaria-driver build  <seforim.db> <index dir>
target/release/otzaria-driver search <seforim.db> <index dir> --mode exact <query...>
  [--mode exact|advanced|fuzzy] [--distance N] [--options k1,k2] [--exclude "words"]
  [--match all|any|most|atleast] [--match-count N] [--limit N]
```

build prints `indexed books N / M (T s)` per batch of 200, `  stage: <name>: <s>s`
(catalogue, open, add books, commit, optimize, book order), `build: <lines> lines, ... in <s>s`,
`index size: <MB> MB in <n> files` and `  .<ext> <MB> MB` per extension (largest first).
Exit 1 on error, 2 on an engine panic; failed books are listed on stderr and make exit 1.

search prints `open: <ms> ms` and `search: <total> hits in <ms> ms (first batch <ms> ms)` to
stderr (one engine call, so both numbers are the same call), then one JSONL line per hit:
`{"book": <id from file_path id:N>, "segment": N, "title", "reference", "text": <snippet html>}`.
`segment` = 0-based row position by `lineIndex` within the book (TextStorage::LibraryDb).

## API used (verified in the v5.0.0 source)

- `configure_line_source(db_path: String) -> Result<()>` — src/api/search_engine.rs:3354
- `SearchEngine::new(path: &str) -> Self` (panics on open failure) — :4842
- `add_library_books(&self, Vec<LibraryBookInput>, &IndexingCancellationToken) -> Result<Vec<LibraryBookResult>>` — :8495; types src/api/library_indexing.rs:12-113
- `open_index_writer(&mut self)` — :8508; `commit(&mut self)` — :8814; `optimize(&mut self)` — :9189
- `set_book_order(&self, Vec<BookOrderEntry>) -> Result<u64>` — :8844 (struct :284)
- `search_and_count_exact` — :9662; `search_and_count_advanced` — :10205; `search_and_count_fuzzy` — :10841
- `split_query_words(query: String) -> Vec<String>` — :3554 (what the app's splitQueryWords calls)
- option keys: src/hebrew_query.rs:48-80 (pass the Hebrew key strings to `--options`)

## Ports and assumptions

- **Catalogue order**: port of `SeforimDbBookTreeOrder.Compute` (C#): categories by
  `level, (orderIndex<0), orderIndex`; books by `orderIndex, id` (C# uses orderIndex only, the
  Dart demo adds id); per folder: base texts, subfolders, rest; negative ids last per level;
  books with a missing category last. Base text = `isBaseBook`, or not dependent
  (`dependenceType` set) and title tokens equal the folder's (leading ספר/מדרש/מסכת dropped).
  Deviation: title tokenizing is simplified (letters/digits, lowercase, שלחן→שולחן); the
  catalog abbreviation map and daf/amud step are not ported (affects only quoted-acronym titles).
  The Dart demo (`seforim_db.dart`) uses isBaseBook only; the C# rule was chosen as "ours".
- **set_book_order**: `catalogue_order` = position in that order, `generation_order` = 0.
  Books are also added in that order.
- **LibraryBookInput** (mirrors indexing_repository.dart:1099-1115): source Library,
  `db_title` = `title` = book.title, `file_path` = `id:<id>`, `text_storage` LibraryDb,
  `topics` = `<category path>/id:<id>` with the path built as `Category.path`
  (library.dart:33, stops at 'ספריית אוצריא') and `BookFacet._normalizeCategoryPath`.
  Assumed: `externalLibraryId` null for official books (key `id:N`); `extra_facets` None
  (the app's BookFacetMetadataCache facets are not reproduced); `source_hash` None.
- **Search**: no facets (whole library), `ResultsOrder::Catalogue`, offset 0, limit 2000,
  no grouping, match_nikud/taamim false. Options = the app's `expandGlobalOptionsToWords`
  (same map for every word, key `{word}_{index}`) for the query and, separately, the exclude
  words. Advanced: `distance` used for both positive and negative distance, scope WordDistance,
  no custom spacing / alternative words, any_order None. Default `--distance` 0 (app default);
  fuzzy defaults to 1 when `--distance` is not given (app clamps its distance to kMaxFuzzyDistance).
- Translation / acronym dictionaries are not loaded (`set_*_dictionary_path` not called), so
  the תרגום ארמי / ראשי תיבות options expand nothing.
