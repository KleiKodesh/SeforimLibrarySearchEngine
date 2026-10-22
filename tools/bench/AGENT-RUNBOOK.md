# Agent runbook: run the benchmarks, report, push

For an AI agent asked to "run the benchmarks" on a fresh clone. Follow it end to end.
Background: [README.md](README.md) (what is tested and why). Repo rules: `CLAUDE.md`.

## Rules

- Inventory FIRST, reuse everything that exists (tools, DB, binaries, indexes of the current format, finished results). Never redo a finished unit unless asked.
- One heavy job at a time. Timing runs strictly sequential; nothing CPU/disk heavy in parallel (no builds, no other agents' runs). Close browsers/IDEs if possible and note load in the report.
- Every long run: full log (run-all does it: `OUT_DIR/logs/<unit>.log`, last line `EXIT <code> (<min>)`), watch progress, read the log's error on failure.
- Units: ms below one second, minutes otherwise; never seconds.
- On NetFree networks (`API Error: 418`): no Hebrew in your output, prompts or subagent reports; refer to queries by set + number.
- Commits: explicit-path staging only, never `git add -A`; no AI attribution / co-author line ever; QA subagent reviews the staged diff first; never commit indexes, DB or OUT_DIR.

## Steps

1. `git submodule update --init --recursive` (if `detect` says a submodule is missing).
2. `tools/bench/run-all.sh --steps detect` -> read the inventory and plan. Missing tools: `--steps bootstrap` (or install by hand, see README section 3).
3. `tools/bench/run-all.sh --steps db` (finds or downloads seforim.db; ~1.9 GB download, resumable).
4. `tools/bench/run-all.sh --steps build-tools` (15-25 min first time). Must end with `all requested steps done`.
5. `tools/bench/run-all.sh --steps build-indexes` (~1 h total; run in the background, watch `OUT_DIR/logs/build-*.log`).
6. `--steps parity` (~30 min). Every compare log must say identical; a difference is analysed (our gap or FtsLib bug) before anything is fixed, and reported.
7. `--steps speed` (1-3 h). Nothing else on the machine.
8. `--steps hitcount` (~10 min). New non-zero diffs come out `unclassified`: analyse (diff lines, read samples) and add the class + reason to `hitcount-classes.json`.
9. `--steps report`. Add the machine (from `OUT_DIR/machine.txt`: CPU, cores, RAM, disk, OS) to FTS-SPECS-AND-COMPARISONS.md if the generator does not yet (TODO below).
10. Commit FTS-SPECS-AND-COMPARISONS.md, README.md, CLAUDE.md (+ hitcount-classes.json if changed) and push.

Recovery: rerun the same command; finished units are skipped (`OUT_DIR/.done/`). `--force` redoes all requested units; delete one marker to redo one unit. An index is rebuilt automatically when its format string changed.

## Slowness analysis (when ours is slower than FtsLib or Otzaria on any query or stage)

- Use the per-query stages in `sb-<set>-pos/search-report.md`: weight, walks+collect, host (text fetch + locate + widening), first batch, total; compare with FtsLib/Otzaria first batch and total.
- Profile the first page: `SeforimSearch.Demo search --index <idx> --db <db> --repeat 3 <query>` with `SEFORIM_ENGINE_TIMING=1` (cold vs warm: search 1 vs later), and the stage split collect / sort / host / locate.
- Find the cause with evidence (numbers, logs), write it in an analysis section of the report, propose fixes. Do NOT change engine code without the user's OK.

## TODO for the next agent (not finished in the first version)

- macOS/Linux FtsLibDump: the portable mode (`-p:FtsLibDumpPortable=true`, default off Windows) compiles Core's `SeforimDbFullTextSearch`, `SeforimDb`, `Common` sources, but Core's `SeforimDb` uses `Settings` and `SeforimDbCatalog` namespaces: add the needed files (or stubs) until it builds on net10.0, exclude Windows-only files (registry, NTFS). Better: ask the user to make Core's FTS part a plain net10.0 project in KitveiHakodesh.
- Not tested on a real Mac/Linux: bootstrap (brew/apt), `.dylib/.so` loading by the demo (`dotnet build -f net10.0 -p:PlatformTarget=AnyCPU`), the rust-lld linker setting (Windows-only target, should not apply), Otzaria driver build, DB download + zstd unpack (tested only the find path).
- `get_db.py` download path not run end to end (needs 1.9 GB); the split-manifest branch untested.
- `specs_report.py`: record `machine.txt` (CPU, cores, RAM, OS) in the generated md; its FtsLib / Otzaria build rows are still hard-coded numbers: parse `logs/build-ftslib.log` and `logs/build-otzaria.log`.
- `specs_report.py` parity line is hard-coded "all identical": read the compare logs.
- `search_report.py` does not pass `--db` to the demo: ours then resolves the DB itself (Otzaria/Zayit under ApplicationData), which misses `--db` paths and the macOS location. Add a `--db` pass-through.
- hitcount: the old classification was made on tantivy9/fts8; rows may change.
- Not yet verified: a full run through run-all (only `detect`, `build-tools` and `--dry-run` were run).
