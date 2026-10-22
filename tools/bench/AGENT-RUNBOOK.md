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

1. `git submodule update --init --depth 1 tantivy external/KleiKodeshProject` (if `detect` says a submodule is missing; not `--recursive`, see the notes below).
2. `tools/bench/run-all.sh --steps detect` -> read the inventory and plan. Missing tools: `--steps bootstrap` (or install by hand, see README section 3).
3. `tools/bench/run-all.sh --steps db` (finds or downloads seforim.db; ~1.9 GB download, resumable).
4. `tools/bench/run-all.sh --steps build-tools` (15-25 min first time). Must end with `all requested steps done`.
5. `tools/bench/run-all.sh --steps build-indexes` (~1 h total; run in the background, watch `OUT_DIR/logs/build-*.log`).
6. `--steps parity` (~30 min). Every compare log must say identical; a difference is analysed (our gap or FtsLib bug) before anything is fixed, and reported.
7. `--steps speed` (1-3 h). Nothing else on the machine.
8. `--steps hitcount` (~10 min). New non-zero diffs come out `unclassified`: analyse (diff lines, read samples) and add the class + reason to `hitcount-classes.json`.
9. `--steps report`. The generator writes the machine (`OUT_DIR/machine.txt`: OS, CPU, cores, RAM, disk), the FtsLib / Otzaria build rows (from `logs/build-ftslib.log`, `logs/build-otzaria.log`; reference numbers only when a log is missing) and the parity table (from the compare logs) into FTS-SPECS-AND-COMPARISONS.md; check them.
10. Commit FTS-SPECS-AND-COMPARISONS.md, README.md, CLAUDE.md (+ hitcount-classes.json if changed) and push.

Recovery: rerun the same command; finished units are skipped (`OUT_DIR/.done/`). `--force` redoes all requested units; delete one marker to redo one unit. An index is rebuilt automatically when its format string changed.

## Slowness analysis (when ours is slower than FtsLib or Otzaria on any query or stage)

- Use the per-query stages in `sb-<set>-pos/search-report.md`: weight, walks+collect, host (text fetch + locate + widening), first batch, total; compare with FtsLib/Otzaria first batch and total.
- Profile the first page: `SeforimSearch.Demo search --index <idx> --db <db> --repeat 3 <query>` with `SEFORIM_ENGINE_TIMING=1` (cold vs warm: search 1 vs later), and the stage split collect / sort / host / locate.
- Find the cause with evidence (numbers, logs), write it in an analysis section of the report, propose fixes. Do NOT change engine code without the user's OK.

## Notes from the first full run on a Mac (Apple M4, macOS, 2026-10-22)

The whole chain ran end to end on macOS arm64: build-tools, build-indexes, parity (all sets identical), speed (1.5 h), hitcount, report.
What broke and how it was solved:

- **Hebrew locale (`he-US` / `he-IL`)**: .NET formats `-8192` as U+200E + `-8192`, so `"PRAGMA cache_size=" + -8192` is a SQLite syntax error (`near "-"`) in our demo and in the pinned FtsLib (all four index builds died in 0 min, exit 134). `run-all.sh` now exports `LC_ALL=en_US.UTF-8` (not on Windows); our `SqliteConnectionFactory` formats with `InvariantCulture`. The FtsLib submodule is left untouched (it still has the bug: fix it upstream with `CultureInfo.InvariantCulture`).
- **Submodules**: `git submodule update --init --recursive` fails on `external/KleiKodeshProject` (its nested `SearchExpansion` has no url in `.gitmodules`); use `git submodule update --init --depth 1 tantivy external/KleiKodeshProject` (non-recursive; nothing needs the nested one). If a first update was interrupted, `tantivy/` can be checked out empty (all files "deleted" in its index): `git -C tantivy reset --hard HEAD`.
- **.NET 10**: `bootstrap` installs it with `brew install --cask dotnet-sdk` (needs sudo, also upgrades git / python / zstd). Without sudo: `dotnet-install.sh --channel 10.0 --install-dir ~/.dotnet10`, then `export DOTNET_ROOT=~/.dotnet10 PATH=~/.dotnet10:$PATH`.
- **seforim.db**: found Zayit's (8 GB, 7,019,636 lines in the index), not Otzaria's; the numbers are not comparable with runs on another DB or machine.
- `tools/bench/hitdiff.py` classifies every differing hit-count row line by line (ours-only / Otzaria-only lines against Otzaria's own tag-stripping rule); rerun it after a new hitcount run and update `hitcount-classes.json` from its output.
- The slowness analysis is `tools/report/slowness-analysis.md` (included by the report generator); rewrite its numbers after every speed run.

## TODO for the next agent

- Not yet run on Linux (the Rust side compiles for macOS and Windows; `cfg(windows)` code does not exist in our crates).
- `get_db.py` download path not run end to end (needs 1.9 GB); the split-manifest branch untested.
- The pinned FtsLib culture bug (above) should be fixed upstream and the submodule bumped; the `LC_ALL` export can then stay as a safety net.
- Speed: ours has no top-K first page (first batch grows with the hit count) and the host path checks distance on text instead of using the positions index; both are proposals in `tools/report/slowness-analysis.md`, no engine change made.
