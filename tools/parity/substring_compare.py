"""Compares two substring dumps (FtsLibDump substring / SeforimSearch.Demo substring).

python -I -X utf8 tools/parity/substring_compare.py ftslib.txt ours.txt
Prints only counts and case numbers (queries as code points), exit 1 on a difference.
"""
import sys

a = open(sys.argv[1], encoding="utf-8").read().splitlines()
b = open(sys.argv[2], encoding="utf-8").read().splitlines()
assert len(a) == len(b), (len(a), len(b))
diffs = 0
for n, (x, y) in enumerate(zip(a, b)):
    if x == y:
        continue
    diffs += 1
    q, rx = x.split("\t", 1)
    _, ry = y.split("\t", 1)
    sx = set(rx.split()) if rx != "DECLINED" else None
    sy = set(ry.split()) if ry != "DECLINED" else None
    cps = " ".join("%04X" % ord(c) for c in q)
    if sx is None or sy is None:
        print(f"case {n} [{cps}]: declined ftslib={sx is None} ours={sy is None}")
    elif sx == sy:
        print(f"case {n} [{cps}]: same {len(sx)} candidates, other order")
    else:
        print(f"case {n} [{cps}]: ftslib {len(sx)}, ours {len(sy)}, only ftslib {len(sx - sy)}, only ours {len(sy - sx)}")
declined = sum(1 for x in a if x.endswith("\tDECLINED"))
print(f"{len(a)} cases, {declined} declined by FtsLib, {diffs} differ")
sys.exit(1 if diffs else 0)
