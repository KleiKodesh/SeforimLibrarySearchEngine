"""Search timing report: runs every query of a query file through the C# demo
with the engine's stage timing on, keeps the FULL log, and writes a Markdown
report with one row per query.

Usage:
  python -I tools/report/search_report.py <demo exe> <index dir> <queries.txt> <out dir>
  python -I tools/report/search_report.py <FtsLibDump exe> <ftslib index> <queries.txt> <out dir> --ftslib <seforim.db>
  the demo form takes [--db <seforim.db>] (passed to the demo; without it the demo
  looks in the Otzaria / Zayit data folders); either form takes [--runs N]

With --ftslib the same queries run through KitveiHakodesh Core's own searcher on
an FtsLib index (FtsLibDump search); FtsLib has no engine stage timing, so only
first batch / total / process are filled for it.

--runs N (N >= 1, default 1): with N > 1, one warm-up run (logged, not counted;
if it fails it is kept as the failing sample and no timed runs follow), then N
runs per query; each stage is the median of the N (mean of the two middle values
for even N). Every run is appended to search-<n>.log.

Load mode (issue #31):
  python -I tools/report/search_report.py <demo exe> <index dir> <queries.txt> <out dir> --load N
      [--db <seforim.db>] [--rounds R] [--commit-every MS] [--update-lines K]
  N concurrent searchers (SeforimSearch.Demo load) each run every query R times (default 3)
  while one writer re-indexes K lines (default 100; delete + add of the same documents) and
  commits every MS ms (default 1000). The updates change the index, so the index is COPIED
  to <out dir>/load-index first and the copy is deleted afterwards. Per query: first-batch
  and total latency p50 / p95 / max over all its searches, and whether its hit count stayed
  the same (it must: the updates rewrite identical lines); per commit: latency p50 / p95 /
  max. Writes load-report.md / load-report.json and the full log load.log in <out dir>.

Stages per query (all in ms):
  weight     engine: build the query weight (SEFORIM_ENGINE_TIMING "weight");
             cheap: the dictionary walks do NOT run here
  collect    engine: per segment, the FST walks / FM lookups + caps (they run
             when tantivy builds the scorers) and the postings walk that
             collects every matching line
  host       host: the rest of the search call = fetch line texts from seforim.db
             in FtsLib's batch plan, locate snippets (engine, multi-core),
             neighbour widening (host SQL + locate)
  first      time to the first batch of results (what the user waits for).
             One definition for every engine: from the search call to the first
             page (256 hits with line text + snippet) in the caller's hands, in a
             process that has already opened the index (ours: SeforimSearch.Demo
             search opens it before the clock; FtsLib: FtsLibDump creates the
             engine before the clock; Otzaria: the driver opens its index and
             line source before the clock, first batch = a --limit 256 call).
             The seforim.db connection is opened inside the first search where
             the engine does so (ours: pooled, so only the first search pays).
             --repeat N (demo, FtsLibDump, driver) runs N searches in one
             process: search 1 = cold, later ones = warm.
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


def main(exe, index, queries, out, ftslib_db=None, runs=1, db=None):
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
            if db:
                args += ["--db", db]
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


def pct(values, p):
    """Nearest-rank percentile (p in 0..100) of `values` (None when empty)."""
    v = sorted(values)
    if not v:
        return None
    k = -(-p * len(v) // 100) - 1
    return v[max(0, min(len(v) - 1, int(k)))]


def load_main(exe, index, queries, out, searchers, db=None, rounds=3, commit_every=1000, update_lines=100):
    import json
    import shutil
    out = Path(out)
    out.mkdir(parents=True, exist_ok=True)
    copy = out / "load-index"
    if copy.exists():
        shutil.rmtree(copy)
    print(f"load: copying the index to {copy}", flush=True)
    shutil.copytree(index, copy)
    args = [exe, "load", "--index", str(copy), "--queries", queries, "--searchers", str(searchers),
            "--rounds", str(rounds), "--commit-every", str(commit_every), "--update-lines", str(update_lines)]
    if db:
        args += ["--db", db]
    qlines = [l for l in Path(queries).read_text(encoding="utf-8").splitlines()
              if l.strip() and not l.startswith("#")]
    expected = searchers * rounds * len(qlines)
    log = out / "load.log"
    events = []
    t0 = time.perf_counter()
    with log.open("w", encoding="utf-8") as lf:
        lf.write(f"$ {' '.join(args)}\n\n")
        p = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        done = 0
        for raw in p.stdout:
            line = raw.decode("utf-8", "replace").rstrip("\r\n")
            lf.write(line + "\n")
            lf.flush()
            events.append(line)
            if line.startswith("load-search "):
                done += 1
                if done % max(1, expected // 20) == 0 or done == expected:
                    print(f"load: {done}/{expected} searches ({(time.perf_counter() - t0) * 1000:,.0f} ms)", flush=True)
            elif "error=" in line or line.startswith("load:"):
                print(line.encode("ascii", "replace").decode(), flush=True)
        code = p.wait()
        lf.write(f"\nexit {code}\n")
    wall = (time.perf_counter() - t0) * 1000
    shutil.rmtree(copy, ignore_errors=True)

    def kv(line):
        head, _, err = line.partition(" error=")
        d = dict(part.split("=", 1) for part in head.split()[1:] if "=" in part)
        if err:
            d["error"] = err
        return d

    per_q = {}
    commits = []
    errors = []
    for line in events:
        if line.startswith("load-search "):
            e = kv(line)
            q = per_q.setdefault(int(e["q"]), {"first": [], "total": [], "hits": set(), "n": 0})
            q["n"] += 1
            q["hits"].add(int(e["hits"]))
            if e.get("first", "-") != "-":
                q["first"].append(float(e["first"]))
            q["total"].append(float(e["total"]))
            if "error" in e:
                errors.append(line)
        elif line.startswith("load-commit "):
            e = kv(line)
            if "ms" in e:
                commits.append(float(e["ms"]))
            if "error" in e:
                errors.append(line)

    def stats(v):
        return {"p50": pct(v, 50), "p95": pct(v, 95), "max": max(v) if v else None}

    def f(v):
        """ms below one second, minutes otherwise (the report rule)."""
        if v is None:
            return "-"
        return f"{v:,.0f} ms" if v < 1000 else f"{v / 60000:.2f} min"

    rows = []
    for n, line in enumerate(qlines):
        q = per_q.get(n, {"first": [], "total": [], "hits": set(), "n": 0})
        rows.append(dict(n=n, query=line.partition("\t")[0], searches=q["n"],
                         hits=sorted(q["hits"]), hits_stable=len(q["hits"]) <= 1,
                         first_ms=stats(q["first"]), total_ms=stats(q["total"])))
    stamp = datetime.datetime.now().strftime("%Y-%m-%d %H:%M")
    md = [f"# Search load report ({stamp})", "",
          f"{searchers} concurrent searchers x {rounds} rounds x {len(qlines)} queries; one writer re-indexes "
          f"{update_lines} lines and commits every {commit_every} ms. Index: copy of `{index}`. Runner: `{exe}`. "
          f"Exit {code}; wall {f(wall)}.", "",
          "| # | query | searches | hits | hits stable | first p50 | first p95 | first max | total p50 | total p95 | total max |",
          "|---|---|---|---|---|---|---|---|---|---|---|"]
    for r in rows:
        md.append(f"| {r['n']} | {r['query']} | {r['searches']} | {', '.join(map(str, r['hits'])) or '-'} | "
                  f"{'yes' if r['hits_stable'] else 'NO'} | {f(r['first_ms']['p50'])} | {f(r['first_ms']['p95'])} | "
                  f"{f(r['first_ms']['max'])} | {f(r['total_ms']['p50'])} | {f(r['total_ms']['p95'])} | {f(r['total_ms']['max'])} |")
    cs = stats(commits)
    md += ["", f"Commits: {len(commits)}, latency p50 {f(cs['p50'])}, p95 {f(cs['p95'])}, max {f(cs['max'])}.",
           f"Errors: {len(errors)}." + (" See load.log." if errors else ""), "",
           "first = search call to the first results frame; total = the whole search call (every hit read). "
           "Full log: load.log next to this report."]
    (out / "load-report.json").write_text(json.dumps({
        "searchers": searchers, "rounds": rounds, "commit_every_ms": commit_every, "update_lines": update_lines,
        "index": index, "exit": code, "wall_ms": wall, "errors": len(errors),
        "commits": {"n": len(commits), **cs}, "rows": rows,
    }, ensure_ascii=False, indent=1), encoding="utf-8")
    (out / "load-report.md").write_text("\n".join(md) + "\n", encoding="utf-8")
    print(f"report: {out / 'load-report.md'}", flush=True)
    unstable = [r for r in rows if not r["hits_stable"]]
    return 1 if code != 0 or errors or unstable else 0


if __name__ == "__main__":
    a = sys.argv[1:]
    ftslib_db = None
    if "--ftslib" in a:
        i = a.index("--ftslib")
        ftslib_db = a[i + 1]
        del a[i:i + 2]
    demo_db = None
    if "--db" in a:
        i = a.index("--db")
        demo_db = a[i + 1]
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
    def int_opt(name, default):
        if name not in a:
            return default
        i = a.index(name)
        try:
            v = int(a[i + 1])
        except (IndexError, ValueError):
            sys.exit(f"{name} needs a whole number >= 1")
        if v < 1:
            sys.exit(f"{name} needs a whole number >= 1")
        del a[i:i + 2]
        return v
    load_n = int_opt("--load", 0)
    if load_n:
        if ftslib_db:
            sys.exit("--load runs our engine only (no --ftslib)")
        sys.exit(load_main(*a[:4], searchers=load_n, db=demo_db, rounds=int_opt("--rounds", 3),
                           commit_every=int_opt("--commit-every", 1000),
                           update_lines=int_opt("--update-lines", 100)))
    sys.exit(main(*a[:4], ftslib_db=ftslib_db, runs=runs, db=demo_db))
