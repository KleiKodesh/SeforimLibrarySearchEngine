"""Otzaria v5.0.0 engine search timing over a parity query file, written in the
search-report.json shape so tools/report/compare_report.py can chart it.
usage: python -I otz_report.py <driver exe> <db> <index> <queries.txt> <out dir> [runs]
Translation (FtsLib syntax -> Otzaria):
  one word, no operator           -> exact
  N words, no TAB distance        -> advanced, distance 1000 (any order in the line)
  N words, TAB distance d         -> advanced, distance d
  word~k                          -> fuzzy, distance k
  * ? % | and quoted phrases      -> n/a (no equivalent)
first batch = a run with --limit 256 (FtsLib's first batch); total = --limit 10000000 (every hit).
Prints ASCII only."""
import json, re, subprocess, sys, time
from pathlib import Path


def translate(q, d):
    w = q.split()
    if any(c in q for c in "*?%|") or q.startswith('"') or '" ' in q:
        return None, "wildcard/grammar/OR/phrase: no Otzaria equivalent"
    if "~" in q:
        if len(w) != 1:
            return None, "multi-word fuzzy"
        word, _, k = q.partition("~")
        return ["--mode", "fuzzy", "--distance", k or "1", word], None
    if d:
        return ["--mode", "advanced", "--distance", d] + w, None
    if len(w) == 1:
        return ["--mode", "exact"] + w, None
    return ["--mode", "advanced", "--distance", "1000"] + w, None


def med(v):
    v = sorted(x for x in v if x is not None)
    return v[len(v) // 2] if v else None


def main(exe, db, index, queries, out, runs="3"):
    runs = int(runs)
    out = Path(out); out.mkdir(parents=True, exist_ok=True)
    lines = [l for l in Path(queries).read_text(encoding="utf-8").splitlines()
             if l.strip() and not l.startswith("#")]
    rows = []
    for n, line in enumerate(lines):
        q, _, d = line.partition("\t")
        d = d.strip()
        args, why = translate(q, d)
        if args is None:
            rows.append(dict(n=n, query=q, dist=d or "-", exit=0, na=why, engine_hits=None, hits=None,
                             weight_ms=None, walks_collect_ms=None, host_ms=None, first_ms=None,
                             total_ms=None, process_ms=None))
            print(f"[{n + 1}/{len(lines)}] n/a ({why})", flush=True)
            continue
        log = out / f"search-{n}.log"
        log.write_text("", encoding="utf-8")
        tot, fst, wall, hits, code = [], [], [], None, 0
        single = False
        for lim, sink in (("10000000", tot), ("256", fst)):
            r = 0
            while r <= runs:
                a = [exe, "search", db, index, "--limit", lim] + args
                t0 = time.perf_counter()
                p = subprocess.run(a, capture_output=True)
                ms = (time.perf_counter() - t0) * 1000
                nl = p.stdout.count(10)
                err = p.stderr.decode("utf-8", "replace")
                with log.open("a", encoding="utf-8") as f:
                    f.write(f"=== run {r} limit {lim}\n$ {' '.join(a)}\nexit {p.returncode}\n"
                            f"stdout lines {nl}\n{err}\n")
                if p.returncode:
                    code = p.returncode
                m = re.search(r"search: (\d+) hits in ([\d.]+) ms", err)
                if not m:
                    break
                v = float(m.group(2))
                # warm-up (r == 0) is dropped, unless one call takes over 20 s: then that
                # single run is the sample (OS cache is warm from earlier runs anyway)
                if r > 0 or v > 20000:
                    sink.append(v)
                    if sink is tot:
                        hits = int(m.group(1)); wall.append(ms)
                if r == 0 and v > 20000:
                    single = True
                    break
                r += 1
        rows.append(dict(n=n, query=q, dist=d or "-", exit=code, mode=args[1], engine_hits=hits, hits=hits,
                         weight_ms=None, walks_collect_ms=None, host_ms=None,
                         first_ms=med(fst), total_ms=med(tot), process_ms=med(wall), single_run=single))
        print(f"[{n + 1}/{len(lines)}] {args[1]} exit {code} hits {hits} total {med(tot)} ms first {med(fst)} ms", flush=True)
    (out / "search-report.json").write_text(json.dumps(
        {"engine": "Otzaria search_engine v5.0.0", "index": index, "rows": rows}, ensure_ascii=False, indent=1),
        encoding="utf-8")
    bad = [r["n"] for r in rows if r["exit"]]
    print(f"done, failed: {bad}", flush=True)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(*sys.argv[1:]))
