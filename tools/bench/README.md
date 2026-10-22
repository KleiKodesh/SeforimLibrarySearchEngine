# Benchmarks on any machine

One command runs the whole comparison on the full `seforim.db`: index builds, FtsLib
parity, search speed, Otzaria hit counts, and the generated report
([FTS-SPECS-AND-COMPARISONS.md](../../FTS-SPECS-AND-COMPARISONS.md), README / CLAUDE.md Specs blocks).

```
git clone https://github.com/KleiKodesh/SeforimLibrarySearchEngine.git
cd SeforimLibrarySearchEngine
git submodule update --init --depth 1 tantivy external/KleiKodeshProject     # not --recursive (a nested submodule has no url)
tools/bench/run-all.sh --steps detect           # inventory + plan, changes nothing
tools/bench/run-all.sh --steps bootstrap        # install missing prerequisites
tools/bench/run-all.sh                          # everything (resume-safe)
```

Windows: run it in Git Bash, or `powershell -ExecutionPolicy Bypass -File tools\bench\run-all.ps1 <same args>`.
An AI agent follows [AGENT-RUNBOOK.md](AGENT-RUNBOOK.md).

## 1. What is compared

| Engine | What | Version / index format |
|---|---|---|
| ours, positions | this repo: modified tantivy (`tantivy/` submodule) + hebrew-fts walks; index with positions (span queries for distance) | `TantivySeforimIndex.IndexFormatVersion` (tantivy11 at the time of writing) |
| ours, no positions | same engine; distance/order checked on the line text by the locator | same |
| ours, keep marks | positions index that keeps nikud/te'amim (vocalized walks); build only | same |
| FtsLib | KitveiHakodesh's FtsLib (SQLite FTS5, `detail=none`) through Core's own searcher (`tools/parity/FtsLibDump`) | submodule `external/KleiKodeshProject` pinned at 1c647ea7, `SeforimDbFtsIndexer.IndexFormatVersion` (fts9) |
| Otzaria | `otzaria_search_engine` v5.0.0, unmodified, through `tools/bench/otzaria-driver` (cargo git dependency, tag v5.0.0) | Otzaria's own tantivy index |

Feature specs and the latest numbers: [FTS-SPECS-AND-COMPARISONS.md](../../FTS-SPECS-AND-COMPARISONS.md).

## 2. What is tested and why

| Step | What | Why |
|---|---|---|
| `detect` | OS/arch, CPU/RAM/disk, tools, submodules, seforim.db, built binaries, indexes (with their format), finished results; then the plan | never redo work; reuse what exists |
| `bootstrap` | installs git, Rust (rustup), .NET 10 SDK, Python 3, zstd (brew / apt / dnf / winget); `git submodule update --init --depth 1 tantivy external/KleiKodeshProject` | clean machine |
| `db` | finds seforim.db or downloads it (below) | the corpus |
| `build-tools` | `cargo build --release -p engine-ffi`, C# demo, FtsLibDump, Otzaria driver | |
| `build-indexes` | ours pos, nopos, keep_marks; FtsLib; Otzaria, one at a time; each log has total time, stage times (indexing, merge, FM sidecar) and size per file type | build cost and index size |
| `parity` | for every set: FtsLibDump dumps FtsLib's hits (first 20,000), the demo dumps ours (pos and nopos), `tools/parity/compare.py` diffs lines, snippets and marks | the engine must return exactly what FtsLib returns |
| `speed` | every query of every speed set through each engine as a separate process: one warm-up, then RUNS (3) timed runs, median; every hit is read; strictly sequential | speed |
| `hitcount` | ours vs Otzaria hit counts with equivalent options (`hitcount.py`); differences carry the hand classification in `hitcount-classes.json` (our bug / Otzaria bug / deliberate deviation / not equivalent query / mixed), else `unclassified` | Otzaria parity of results |
| `report` | `tools/report/specs_report.py <out> --prefix ""` regenerates the specs md + README/CLAUDE.md blocks; `<out>/machine.txt` holds the machine inventory | one report |

**Timing definitions** (see `tools/report/search_report.py`): total = the whole search call
from the host's side, every hit read; first batch = from the search call to the first page
(256 hits with text + snippet) in the caller's hands, index already open; stages (ours):
weight, walks+collect, host (text fetch + locate + widening). Units: ms below one second,
minutes otherwise.

**Query sets** (`tools/parity/`): `queries-diff10.txt` (10 queries that once differed),
`queries-large.txt` (61: broad words, phrases, wildcards, fuzzy, grammar `%`, OR, distance),
`queries-words.txt` (14: word-model edge cases), `queries-rules.txt` (21: tokenizer rules),
`queries-ketiv.txt` (12, run with `--ketiv`). A TAB after a query gives the word distance.

**Otzaria mapping** (`otz_report.py`): one word = exact; several words = advanced with the TAB
distance, or 1000 (anywhere in the line); `w~k` = fuzzy k; wildcards, `%`, `|` and quoted
phrases have no Otzaria equivalent and are `n/a`.

## 3. What you need

- Disk: ~10 GB outputs (indexes: ours pos ~2.5 GB, nopos, marks, FtsLib ~0.75 GB, Otzaria ~1.9 GB) + seforim.db 3.9 GB (+1.9 GB for the compressed download).
- RAM: 16 GB recommended. CPU: more cores = faster builds.
- Time (8-core laptop): build-tools 15-25 min the first time (tantivy and Otzaria's engine);
  each index 8-20 min; parity ~30 min; speed 1-3 h; hitcount ~10 min.
- Prerequisites: git, Rust stable, .NET 10 SDK, Python 3 (3.14 unpacks zstd natively; older
  needs `pip install zstandard` or the zstd CLI). Windows only: the .NET Framework 4.8
  targeting pack (the demo also builds net48) and the MSVC C++ build tools for Rust.
- Submodules: `tantivy` (our modified copy) and `external/KleiKodeshProject` (KitveiHakodesh:
  FtsLib + Core, ~0.8 GB history; `--depth 1` saves space).
- seforim.db: found at `--db` / `DB_PATH`, else Otzaria's location
  (Windows `%APPDATA%\otzaria\books`, macOS `~/Library/Application Support/otzaria/books`,
  Linux `~/.local/share/otzaria/books`), else Zayit's, else `<out>/data`; otherwise
  downloaded from the latest release of `github.com/Otzaria/SeforimLibrary`, the asset the
  Otzaria app uses (`seforim-schema7.db.zst`, `seforim-schema6.db.zst` or `seforim.db.zst`,
  ~1.9 GB), resumable, size and sha256 checked, unpacked to `<out>/data/seforim.db`. A
  downloaded DB may be newer than the one earlier results were measured on.

### Clean Mac (Apple silicon or Intel)

```sh
xcode-select --install                      # clang + SDK for Rust's C deps (zstd-sys)
# Homebrew from https://brew.sh, then:
git clone https://github.com/KleiKodesh/SeforimLibrarySearchEngine.git
cd SeforimLibrarySearchEngine
tools/bench/run-all.sh --steps bootstrap    # git, python, zstd, rustup, .NET 10 SDK
tools/bench/run-all.sh --steps detect       # inventory: OS, CPU, cores, RAM, disk, tools
tools/bench/run-all.sh                      # everything (or step by step, see section 2)
```

What differs from Windows: the native engine is `libseforim_search_native.dylib` (copied
next to the demo by the engine package); the demo builds only net10.0 (AnyCPU, so arm64 and
x64 both run natively); FtsLibDump builds against `KitveiHakodesh.Core.Portable` (a plain
net10.0 build of Core's own FTS / seforim.db / catalog sources, registry calls guarded) and
lands in `bin/Release/net10.0/`; the DB is looked up under `~/Library/Application Support`.
Measured runs stay sequential; close other apps and keep the laptop on power (macOS
throttles on battery).

## 4. Running

Configuration: `tools/bench/bench.env` (copy to `bench.local.env` for local overrides):
`DB_PATH`, `OUT_DIR` (default `~/seforim-bench`), `KITVEI_DIR`, index dirs, `STEPS`, `RUNS`,
sets. Flags: `--steps a,b`, `--db PATH`, `--out DIR`, `--force`, `--dry-run`.

Outputs in `OUT_DIR`: `logs/` (full log of every unit with `EXIT <code> (<min>)`),
`indexes/`, `parity/<set>-{fts,pos,nopos}`, `sb-<set>-<engine>/` (search-report.md/json +
per-query logs), `otz-hitcount.json`, `machine.txt`, `.done/` (resume markers; delete one to
redo that unit, or `--force`).

Reading the report: FTS-SPECS-AND-COMPARISONS.md has build tables, parity, per-set speed
charts and per-query stage tables, and the hit-count table with classes.

Copying results back: commit `FTS-SPECS-AND-COMPARISONS.md`, `README.md`, `CLAUDE.md`
(the generated blocks) by explicit path. Never commit indexes or the DB. To keep raw runs,
zip `OUT_DIR` without `indexes/` and `data/`.

## Troubleshooting

- `418 I'm a Teapot` / blocked downloads: the NetFree filter. Keep Hebrew out of agent output; download the DB in a browser and pass `--db`.
- .NET Framework 4.8 is Windows-only; on macOS/Linux only net10.0 is built.
- macOS / Linux: FtsLibDump references `KitveiHakodesh.Core.Portable` (net10.0); on Windows it references Core itself (`-p:FtsLibDumpPortable=true` forces the portable build there too; both give identical dumps).
- macOS: `cargo` fails in `zstd-sys` without the Xcode command line tools (`xcode-select --install`).
- Windows long paths: `git config --system core.longpaths true`.
- Hebrew in consoles: scripts print ASCII; use `python -X utf8` for anything that prints Hebrew.
- A failed unit: read its log in `OUT_DIR/logs`, fix, rerun the same command (finished units are skipped).
