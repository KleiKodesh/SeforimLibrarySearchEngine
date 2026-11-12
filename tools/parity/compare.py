"""Compare two parity dumps (FtsLib vs tantivy) query by query.

Usage: python compare.py <ftslib out dir> <tantivy out dir> [--diff-out FILE.json] [--strict hits|all]

--diff-out writes every difference in full (per query: hits only on one side, the order
divergence, and every common hit whose fields differ, both values). --strict sets the exit
code: 1 when a query differs in its hit set / order (hits) or in anything (all). The
positions flavour uses hits: its snippet window is the word-tightest one, a named deviation
(issue #21), so window / mark / distance differences are expected there.

For each query reports: hit counts, the first divergence in hit order (line
id), and per-field mismatches over the hits both sides returned (word
distance, snippet window, flags, marks with their term strings).
"""
import json
import sys
from pathlib import Path

FIELDS = ["dist", "start", "end", "flags", "marks"]


def load(path):
    lines = path.read_text(encoding="utf-8").splitlines()
    header = json.loads(lines[0])
    hits = [json.loads(l) for l in lines[1:]]
    # Our index keeps intra-word quotes (רש"י); FtsLib stored רשי. Compare the
    # marked term strings quote-free (' and "); offsets are compared exactly.
    for h in hits:
        h["marks"] = [[s, l, t.replace('"', "").replace("'", "")] for s, l, t in h["marks"]]
    return header, hits


def main(a_dir, b_dir, diff_out=None, strict=None):
    a_dir, b_dir = Path(a_dir), Path(b_dir)
    files = sorted(a_dir.glob("*.jsonl"), key=lambda p: int(p.stem))
    total_ok = 0
    hits_bad = 0
    diffs = []
    for fa in files:
        fb = b_dir / fa.name
        ha, a = load(fa)
        if not fb.exists():
            print(f"#{fa.stem} {ha['query']!r}: missing on tantivy side")
            hits_bad += 1
            diffs.append({"n": int(fa.stem), "query": ha["query"], "missing_on_tantivy": True})
            continue
        _, b = load(fb)
        order_a = [h["line"] for h in a]
        order_b = [h["line"] for h in b]
        first_div = next((i for i, (x, y) in enumerate(zip(order_a, order_b)) if x != y), None)
        b_by_line = {h["line"]: h for h in b}
        common = [h for h in a if h["line"] in b_by_line]
        mism = {f: 0 for f in FIELDS}
        example = None
        field_diffs = []
        for h in common:
            o = b_by_line[h["line"]]
            fd = {}
            for f in FIELDS:
                if h[f] != o[f]:
                    mism[f] += 1
                    fd[f] = {"ftslib": h[f], "tantivy": o[f]}
                    if example is None:
                        example = (f, h["line"], h[f], o[f])
            if fd:
                field_diffs.append({"line": h["line"], **fd})
        only_a = len(set(order_a) - set(order_b))
        only_b = len(set(order_b) - set(order_a))
        same = (order_a == order_b) and not any(mism.values())
        total_ok += same
        status = "SAME" if same else "DIFF"
        if order_a != order_b:
            hits_bad += 1
        if not same:
            sa, sb = set(order_a), set(order_b)
            diffs.append({"n": int(fa.stem), "query": ha["query"], "distance": ha.get("distance"),
                          "ftslib_hits": len(a), "tantivy_hits": len(b),
                          "only_ftslib": [l for l in order_a if l not in sb],
                          "only_tantivy": [l for l in order_b if l not in sa],
                          "first_order_divergence_at": first_div,
                          "field_mismatch_counts": {k: v for k, v in mism.items() if v},
                          "field_diffs": field_diffs})
        print(f"#{fa.stem} {status} {ha['query']!r} dist={ha['distance']}: "
              f"ftslib={len(a)} tantivy={len(b)} only_ftslib={only_a} only_tantivy={only_b}"
              + (f" first_order_divergence_at={first_div}" if first_div is not None else ""))
        if any(mism.values()):
            print("     field mismatches on common hits:", {k: v for k, v in mism.items() if v})
            f, line, va, vb = example
            print(f"     e.g. line {line} {f}: ftslib={va} tantivy={vb}")
    print(f"\n{total_ok}/{len(files)} queries identical")
    print(f"{len(files) - hits_bad}/{len(files)} queries with identical hits (set and order)")
    if diff_out:
        Path(diff_out).write_text(json.dumps({"queries": len(files), "identical": total_ok,
                                              "identical_hits": len(files) - hits_bad, "diffs": diffs},
                                             ensure_ascii=False, indent=1), encoding="utf-8")
    if strict == "all" and total_ok != len(files):
        return 1
    if strict == "hits" and hits_bad:
        return 1
    return 0


if __name__ == "__main__":
    a = sys.argv[1:]
    opt = lambda k: a[a.index(k) + 1] if k in a else None
    sys.exit(main(a[0], a[1], opt("--diff-out"), opt("--strict")))
