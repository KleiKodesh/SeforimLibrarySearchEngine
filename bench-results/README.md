# bench-results

The full-run results that `tools/report/specs_report.py` reads by default, committed so any
machine or session regenerates FTS-SPECS-AND-COMPARISONS.md and the README / CLAUDE.md specs
blocks with the measured numbers (no argument needed). Heavy outputs (indexes, raw logs,
parity dumps) stay in the bench `OUT_DIR`.

| Path | Written by | Read for |
|---|---|---|
| `logs/build-{pos,nopos,marks,ftslib,otzaria}.log` | run-all `report` step (stage / size / total lines of the build log) | Index builds |
| `logs/parity-<set>-{pos,nopos}-compare.log` | run-all `report` step (compare.py's "n/m queries identical" line) | Parity |
| `sb-<set>-{pos,nopos,fts,otz}/search-report.json` (+ `.md`) | search_report.py / otz_report.py, copied by the `report` step | Search speed |
| `otz-hitcount.json` | hitcount.py, copied by the `report` step | Hit-count comparison |
| `machine.txt` | run-all `detect`, copied by the `report` step | Machine line |

`tools/bench/run-all.sh --steps report` publishes the files a run produced into this folder
(`RESULTS_DIR` in bench.env) and regenerates the docs; files a partial run did not produce keep
their earlier version. After a full-DB rerun on a new index format, update `MEASURED_FTSLIB` and
the `STALE` list in specs_report.py.

**Provenance of the current files:** the raw outputs of the last full runs (`tantivy9` / FtsLib
`fts8`, October 2026) lived in a temporary session scratchpad and are gone. Every file here was
reconstructed from FTS-SPECS-AND-COMPARISONS.md at commit cae7954 (each carries a `_note` / `#`
line saying so): per-query times as printed there (ms, or minutes to two decimals), and the
aggregates as originally printed in a `summary` object, because sums and medians recomputed from
rounded values would drift. A real run writes no `summary`; the generator then computes them.
No `machine.txt` survived.
