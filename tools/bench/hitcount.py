"""Hit-count comparison ours vs Otzaria (search_engine 5babbce) (same queries, equivalent options).

usage: python3 -I -X utf8 tools/bench/hitcount.py <native lib> <ours pos index> <results dir> [seforim.db]
With seforim.db, each row also counts ours over the real books only (`books` = every book id of
the DB): the schema-7 footnotes are indexed as virtual notes books (issue #13), which Otzaria does
not index. A row whose difference is only those notes is classified "notes" automatically.
Reads <results dir>/sb-<set>-otz/search-report.json (otz_report.py), runs ours through the
native library (C ABI, Otzaria-mode options: one word plain; several words ordered
proximity with gaps = the distance (1000 when none) and cross_line; `w~k` fuzzy with
ignore_quotes), writes <results dir>/hc/ours-<set>.json and <results dir>/otz-hitcount.json
(read by tools/report/specs_report.py). Rows with a difference take the hand
classification from hitcount-classes.json when present, else "unclassified".
ASCII output only.
"""
import ctypes as C, json, sqlite3, sys, time
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
    d.se_reduce_paired_readings.restype = C.POINTER(C.c_uint8)
    d.se_reduce_paired_readings.argtypes = [C.c_char_p, C.c_size_t, C.POINTER(C.c_size_t)]
    d.se_bytes_free.argtypes = [C.POINTER(C.c_uint8), C.c_size_t]
    return d


def reduce_paired(d, q):
    """The engine numbers typed words after a paired reading `(X) [Y]` keeps only Y
    (as Otzaria): per-pair gaps are given for the reduced query."""
    b = q.encode(); n = C.c_size_t()
    p = d.se_reduce_paired_readings(b, len(b), C.byref(n))
    if not p:
        return q
    out = C.string_at(p, n.value).decode("utf-8"); d.se_bytes_free(p, n.value)
    return out


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


def main(lib, index, out, db=None):
    out = Path(out); (out / "hc").mkdir(parents=True, exist_ok=True)
    dll = load(lib)
    ix = dll.se_index_open(index.encode(), len(index.encode()))
    if not ix:
        raise SystemExit("index open failed: " + dll.se_last_error().decode("utf-8", "replace").encode("ascii", "replace").decode())
    books = None
    if db:
        con = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
        books = [r[0] for r in con.execute("SELECT id FROM book")]
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
            req = translate(reduce_paired(dll, q), d.strip())
            if req is None:
                continue
            js = json.dumps(req, ensure_ascii=False).encode()
            t0 = time.perf_counter()
            s = dll.se_search(ix, js, len(js))
            if not s:
                e = dll.se_last_error() or b""
                print(f"{st} q{n}: error: " + e.decode("utf-8", "replace").encode("ascii", "replace").decode(), flush=True); continue
            h = dll.se_search_hit_count(s); dll.se_search_close(s)
            ours[n] = h
            diff = h - r["hits"]
            hb = None
            if books is not None:
                jb = json.dumps(dict(req, books=books), ensure_ascii=False).encode()
                sb = dll.se_search(ix, jb, len(jb))
                if sb:
                    hb = dll.se_search_hit_count(sb); dll.se_search_close(sb)
            if diff == 0:
                c, why = "equal", "same count"
            elif hb == r["hits"]:
                c, why = "notes", "only footnote lines (virtual notes books, issue #13; Otzaria does not index them)"
            elif f"{st}:{n}" in cls["classes"]:
                c, why = tuple(cls["classes"][f"{st}:{n}"])
            else:
                c, why = "unclassified", "not analysed yet"
            rows.append(dict(n=n, query=q, dist=(f"(distance {r['dist']})" if r["dist"] != "-" else ""), mode=r.get("mode", ""),
                             otz=r["hits"], ours=h, ours_books=hb, **{"class": c}, reason=why))
            print(f"{st} q{n}: otz {r['hits']} ours {h} diff {diff:+}{'' if hb is None else f' books-only {hb} diff {hb - r['hits']:+}'} ({(time.perf_counter() - t0) * 1000:.0f} ms)", flush=True)
        (out / "hc" / f"ours-{st}.json").write_text(json.dumps(ours), encoding="utf-8")
        res["sets"][st] = rows
    (out / "otz-hitcount.json").write_text(json.dumps(res, ensure_ascii=False, indent=1), encoding="utf-8")
    print("wrote", out / "otz-hitcount.json")


if __name__ == "__main__":
    main(*sys.argv[1:5])
