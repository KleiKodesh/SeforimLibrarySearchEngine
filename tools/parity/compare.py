"""Compare two parity dumps (FtsLib vs tantivy) query by query.

Usage: python compare.py <ftslib out dir> <tantivy out dir>

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


def main(a_dir, b_dir):
    a_dir, b_dir = Path(a_dir), Path(b_dir)
    files = sorted(a_dir.glob("*.jsonl"), key=lambda p: int(p.stem))
    total_ok = 0
    for fa in files:
        fb = b_dir / fa.name
        ha, a = load(fa)
        if not fb.exists():
            print(f"#{fa.stem} {ha['query']!r}: missing on tantivy side")
            continue
        _, b = load(fb)
        order_a = [h["line"] for h in a]
        order_b = [h["line"] for h in b]
        first_div = next((i for i, (x, y) in enumerate(zip(order_a, order_b)) if x != y), None)
        b_by_line = {h["line"]: h for h in b}
        common = [h for h in a if h["line"] in b_by_line]
        mism = {f: 0 for f in FIELDS}
        example = None
        for h in common:
            o = b_by_line[h["line"]]
            for f in FIELDS:
                if h[f] != o[f]:
                    mism[f] += 1
                    if example is None:
                        example = (f, h["line"], h[f], o[f])
        only_a = len(set(order_a) - set(order_b))
        only_b = len(set(order_b) - set(order_a))
        same = (order_a == order_b) and not any(mism.values())
        total_ok += same
        status = "SAME" if same else "DIFF"
        print(f"#{fa.stem} {status} {ha['query']!r} dist={ha['distance']}: "
              f"ftslib={len(a)} tantivy={len(b)} only_ftslib={only_a} only_tantivy={only_b}"
              + (f" first_order_divergence_at={first_div}" if first_div is not None else ""))
        if any(mism.values()):
            print("     field mismatches on common hits:", {k: v for k, v in mism.items() if v})
            f, line, va, vb = example
            print(f"     e.g. line {line} {f}: ftslib={va} tantivy={vb}")
    print(f"\n{total_ok}/{len(files)} queries identical")


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
