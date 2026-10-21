"""Search timing report: runs every query of a query file through the C# demo
with the engine's stage timing on, keeps the FULL log, and writes a Markdown
report with one row per query.

Usage:
  python -I tools/report/search_report.py <demo exe> <index dir> <queries.txt> <out dir>
  python -I tools/report/search_report.py <FtsLibDump exe> <ftslib index> <queries.txt> <out dir> --ftslib <seforim.db>
  either form takes [--runs N] (any position)

With --ftslib the same queries run through KitveiHakodesh Core's own searcher on
an FtsLib index (FtsLibDump search); FtsLib has no engine stage timing, so only
first batch / total / process are filled for it.

--runs N (N >= 1, default 1): with N > 1, one warm-up run (logged, not counted;
if it fails it is kept as the failing sample and no timed runs follow), then N
runs per query; each stage is the median of the N (mean of the two middle values
for even N). Every run is appended to search-<n>.log.

Stages per query (all in ms):
  weight     engine: build the query weight (SEFORIM_ENGINE_TIMING "weight");
             cheap: the dictionary walks do NOT run here
  collect    engine: per segment, the FST walks / FM lookups + caps (they run
             when tantivy builds the scorers) and the postings walk that
             collects every matching line
  host       host: the rest of the search call = fetch line texts from seforim.db
             in FtsLib's batch plan, locate snippets (engine, multi-core),
             neighbour widening (host SQL + locate)
  first      time to the first batch of results (what the user waits for)
  total      whole search call, from the host's side
Every run's full stdout/stderr is in <out dir>/search-<n>.log; the exit code
of each run is in the report, and a non-zero exit is flagged.
"""
import datetime
import os
import re
import subprocess
import sys
import time
from pathlib import Path


def median(values):
    v = sorted(x for x in values if x is not None)
    if not v:
        return None
    h = len(v) // 2
    return v[h] if len(v) % 2 else (v[h - 1] + v[h]) / 2


def main(exe, index, queries, out, ftslib_db=None, runs=1):
    out = Path(out)
    out.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, SEFORIM_ENGINE_TIMING="1")
    rows = []
    lines = [l for l in Path(queries).read_text(encoding="utf-8").splitlines()
             if l.strip() and not l.startswith("#")]
    for n, line in enumerate(lines):
        query, _, dist = line.partition("\t")
        if ftslib_db:
            args = [exe, "search", ftslib_db, index]
        else:
            args = [exe, "search", "--index", index]
        if dist.strip():
            args += ["--distance", dist.strip()]
        args += query.split()
        log = out / f"search-{n}.log"
        log.write_text("", encoding="utf-8")
        samples = []
        # --runs N > 1: one warm-up run (logged, not counted), then N runs; median per stage
        for r in range(runs + (1 if runs > 1 else 0)):
            t0 = time.perf_counter()
            p = subprocess.run(args, capture_output=True, env=env)
            wall = (time.perf_counter() - t0) * 1000
            text = p.stdout.decode("utf-8", "replace") + "\n--- stderr ---\n" + p.stderr.decode("utf-8", "replace")
            label = "warm-up" if runs > 1 and r == 0 else f"run {r if runs > 1 else 1}"
            with log.open("a", encoding="utf-8") as lf:
                lf.write(f"=== {label}\n$ {' '.join(args)}\nexit {p.returncode}\n\n{text}\n")

            def num(pattern):
                m = re.search(pattern, text)
                return float(m.group(1).replace(",", "")) if m else None

            walks = num(r"\[timing\] weight \(walks \+ caps\): ([\d.]+) ms")
            collect = num(r"\[timing\] collect [\d,]+ hits: ([\d.]+) ms")
            raw_hits = num(r"\[timing\] collect ([\d,]+) hits")
            hits = num(r"search: ([\d,]+) hits in")
            total = num(r"hits in ([\d.]+) ms")
            first = num(r"first batch ([\d.]+) ms")
            host = total - (walks or 0) - (collect or 0) if total is not None else None
            if runs > 1 and r == 0 and p.returncode == 0:
                continue
            samples.append((p.returncode, raw_hits, hits, walks, collect, host, first, total, wall))
            if p.returncode != 0:
                break
        code = next((s_[0] for s_ in samples if s_[0] != 0), 0)
        cols = [median([s_[i] for s_ in samples]) for i in range(1, 9)]
        rows.append((n, query, dist.strip() or "-", code, *cols))
        total = cols[6]
        status = "OK" if code == 0 else f"FAILED exit {code} (see {log.name})"
        print(f"[{n + 1}/{len(lines)}] {query}: {status}, total {total} ms", flush=True)

    def f(v, digits=0):
        return "-" if v is None else f"{v:,.{digits}f}"

    stamp = datetime.datetime.now().strftime("%Y-%m-%d %H:%M")
    size = sum(f2.stat().st_size for f2 in Path(index).iterdir() if f2.is_file())
    engine = "FtsLib (KitveiHakodesh Core searcher)" if ftslib_db else "tantivy engine (SeforimSearch.Demo)"
    md = [f"# Search timing report ({stamp})", "",
          f"Engine: {engine}. Runs per query: {runs}" + (" (median, after one warm-up)" if runs > 1 else "") + f". Index: `{index}` ({size / 1e6:,.1f} MB). Runner: `{exe}`.", "",
          "| # | query | dist | exit | engine hits | shown hits | weight ms | walks + collect ms | host ms | first batch ms | total ms | process ms |",
          "|---|---|---|---|---|---|---|---|---|---|---|---|"]
    for (n, q, d, code, raw, hits, w, c, h, first, total, wall) in rows:
        md.append(f"| {n} | {q} | {d} | {code} | {f(raw)} | {f(hits)} | {f(w, 1)} | {f(c, 1)} | {f(h)} | {f(first)} | {f(total)} | {f(wall)} |")
    failed = [r for r in rows if r[3] != 0]
    md += ["", f"{len(rows)} queries, {len(failed)} failed." + (" Failed: " + ", ".join(str(r[0]) for r in failed) if failed else ""),
           "", "Stages: weight = query weight build (engine); walks + collect = FST walks / FM lookups + caps and postings to hits (engine); "
           "host = text fetch from seforim.db + snippet locate + neighbour widening; process = whole process incl. start-up and index open.",
           "Full logs: search-<n>.log next to this report."]
    import json
    (out / "search-report.json").write_text(json.dumps({
        "engine": engine, "index": index, "index_bytes": size, "runs": runs,
        "rows": [dict(n=r[0], query=r[1], dist=r[2], exit=r[3], engine_hits=r[4], hits=r[5],
                      weight_ms=r[6], walks_collect_ms=r[7], host_ms=r[8], first_ms=r[9],
                      total_ms=r[10], process_ms=r[11]) for r in rows],
    }, ensure_ascii=False, indent=1), encoding="utf-8")
    report = out / "search-report.md"
    report.write_text("\n".join(md) + "\n", encoding="utf-8")
    print(f"report: {report}", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    a = sys.argv[1:]
    db = None
    if "--ftslib" in a:
        i = a.index("--ftslib")
        db = a[i + 1]
        del a[i:i + 2]
    runs = 1
    if "--runs" in a:
        i = a.index("--runs")
        try:
            runs = int(a[i + 1])
        except (IndexError, ValueError):
            sys.exit("--runs needs a whole number >= 1")
        if runs < 1:
            sys.exit("--runs needs a whole number >= 1")
        del a[i:i + 2]
    sys.exit(main(*a[:4], ftslib_db=db, runs=runs))
