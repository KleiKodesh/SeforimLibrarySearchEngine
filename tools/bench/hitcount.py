"""Hit-count comparison ours vs Otzaria v5.0.0 (same queries, equivalent options).

usage: python3 -I -X utf8 tools/bench/hitcount.py <native lib> <ours pos index> <results dir>
Reads <results dir>/sb-<set>-otz/search-report.json (otz_report.py), runs ours through the
native library (C ABI, Otzaria-mode options: one word plain; several words ordered
proximity with gaps = the distance (1000 when none) and cross_line; `w~k` fuzzy with
ignore_quotes), writes <results dir>/hc/ours-<set>.json and <results dir>/otz-hitcount.json
(read by tools/report/specs_report.py). Rows with a difference take the hand
classification from hitcount-classes.json when present, else "unclassified".
ASCII output only.
"""
import ctypes as C, json, sys, time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
SETS = ("large", "words", "rules")


def load(lib):
    d = C.CDLL(lib)
    d.se_index_open.restype = C.c_void_p; d.se_index_open.argtypes = [C.c_char_p, C.c_size_t]
    d.se_search.restype = C.c_void_p; d.se_search.argtypes = [C.c_void_p, C.c_char_p, C.c_size_t]
    d.se_search_hit_count.restype = C.c_uint64; d.se_search_hit_count.argtypes = [C.c_void_p]
    d.se_last_error.restype = C.c_char_p
    d.se_search_close.argtypes = [C.c_void_p]
    return d


def translate(q, d):  # mirrors otz_report.translate
    w = q.split()
    if any(c in q for c in "*?%|") or q.startswith('"') or '" ' in q:
        return None
    if "~" in q:
        return {"query": q, "ignore_quotes": True} if len(w) == 1 else None
    if len(w) == 1 and not d:
        return {"query": q}
    gap = int(d) if d else 1000
    return {"query": q, "proximity": {"mode": "ordered", "gaps": [gap] * (len(w) - 1)}, "cross_line": True}


def main(lib, index, out):
    out = Path(out); (out / "hc").mkdir(parents=True, exist_ok=True)
    dll = load(lib)
    ix = dll.se_index_open(index.encode(), len(index.encode()))
    if not ix:
        raise SystemExit("index open failed: " + dll.se_last_error().decode("utf-8", "replace").encode("ascii", "replace").decode())
    cls = json.loads((HERE / "hitcount-classes.json").read_text(encoding="utf-8"))
    res = {"method": cls["method"], "notes": cls["notes"], "sets": {}}
    for st in SETS:
        rep = out / f"sb-{st}-otz" / "search-report.json"
        if not rep.exists():
            print(f"{st}: no Otzaria run ({rep}), skipped", flush=True)
            continue
        otz = json.loads(rep.read_text(encoding="utf-8"))["rows"]
        qs = [l for l in (ROOT / f"tools/parity/queries-{st}.txt").read_text(encoding="utf-8").splitlines()
              if l.strip() and not l.startswith("#")]
        ours, rows = {}, []
        for r in otz:
            if r.get("hits") is None:
                continue
            n = r["n"]; q, _, d = qs[n].partition("\t")
            req = translate(q, d.strip())
            if req is None:
                continue
            js = json.dumps(req, ensure_ascii=False).encode()
            t0 = time.perf_counter()
            s = dll.se_search(ix, js, len(js))
            if not s:
                print(f"{st} q{n}: error", flush=True); continue
            h = dll.se_search_hit_count(s); dll.se_search_close(s)
            ours[n] = h
            diff = h - r["hits"]
            c, why = ("equal", "same count") if diff == 0 else tuple(cls["classes"].get(f"{st}:{n}", ("unclassified", "not analysed yet")))
            rows.append(dict(n=n, query=q, dist=(f"(distance {r['dist']})" if r["dist"] != "-" else ""), mode=r.get("mode", ""),
                             otz=r["hits"], ours=h, **{"class": c}, reason=why))
            print(f"{st} q{n}: otz {r['hits']} ours {h} diff {diff:+} ({(time.perf_counter() - t0) * 1000:.0f} ms)", flush=True)
        (out / "hc" / f"ours-{st}.json").write_text(json.dumps(ours), encoding="utf-8")
        res["sets"][st] = rows
    (out / "otz-hitcount.json").write_text(json.dumps(res, ensure_ascii=False, indent=1), encoding="utf-8")
    print("wrote", out / "otz-hitcount.json")


if __name__ == "__main__":
    main(*sys.argv[1:4])
