"""Full-DB verification of cross-line and sameSection. ASCII output only.
usage: python -I -X utf8 tools/bench/verify_xl_ss.py <native lib> <pos index> <seforim.db> <out json>
Run by tools/bench/run-all.sh (step verify-xl). Exit 1 when sameSection differs from the
recount (missing / extra lines, every id listed in the json), a cross-line search loses an
in-line hit, or a cross-line pair joins two books.
sameSection: recomputed independently from per-word hits + line_toc sections (host's rule:
section = (book, line_toc.tocEntryId), no entry = the line alone); sets must be equal.
cross-line: count with/without, cross-only hits checked: next line same book, next by lineIndex
(skipping no line that holds words is not checked here; reported as gap in lineIndex)."""
import ctypes as C, json, sqlite3, sys, time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
R = 1 << 52


def load(lib):
    d = C.CDLL(lib)
    d.se_index_open.restype = C.c_void_p; d.se_index_open.argtypes = [C.c_char_p, C.c_size_t]
    d.se_search.restype = C.c_void_p; d.se_search.argtypes = [C.c_void_p, C.c_char_p, C.c_size_t]
    d.se_search_hit_count.restype = C.c_uint64; d.se_search_hit_count.argtypes = [C.c_void_p]
    d.se_search_copy_hits.restype = C.c_uint64
    d.se_search_copy_hits.argtypes = [C.c_void_p, C.c_uint64, C.c_uint64, C.POINTER(C.c_uint64)]
    d.se_search_copy_next_lines.restype = C.c_uint64
    d.se_search_copy_next_lines.argtypes = [C.c_void_p, C.c_uint64, C.c_uint64, C.POINTER(C.c_uint64)]
    d.se_last_error.restype = C.c_char_p
    d.se_search_close.argtypes = [C.c_void_p]
    return d


def search(d, ix, req, nexts=False):
    js = json.dumps(req, ensure_ascii=False).encode()
    t0 = time.perf_counter()
    s = d.se_search(ix, js, len(js))
    if not s:
        raise RuntimeError(d.se_last_error().decode("utf-8", "replace").encode("ascii", "replace").decode())
    n = d.se_search_hit_count(s)
    buf = (C.c_uint64 * (3 * max(n, 1)))()
    d.se_search_copy_hits(s, 0, n, buf)
    hits = [(buf[3 * i], buf[3 * i + 1], buf[3 * i + 2]) for i in range(n)]
    nx = None
    if nexts:
        nb = (C.c_uint64 * max(n, 1))()
        d.se_search_copy_next_lines(s, 0, n, nb)
        nx = [nb[i] for i in range(n)]
    d.se_search_close(s)
    return hits, nx, (time.perf_counter() - t0) * 1000


def line_info(db, ids):
    out = {}
    ids = [i for i in ids if i < R]
    for k in range(0, len(ids), 900):
        ch = ids[k:k + 900]
        q = ",".join("?" * len(ch))
        for lid, b, li, toc in db.execute(
                f"SELECT l.id, l.bookId, l.lineIndex, lt.tocEntryId FROM line l LEFT JOIN line_toc lt ON lt.lineId=l.id WHERE l.id IN ({q})", ch):
            out[lid] = (b, li, toc)
    return out


def main(lib, index, dbp, outp):
    d = load(lib)
    ix = d.se_index_open(index.encode(), len(index.encode()))
    if not ix:
        raise SystemExit("index open failed")
    db = sqlite3.connect(f"file:{dbp}?mode=ro", uri=True)
    qs = []
    for st in ("large", "words", "rules"):
        for n, l in enumerate((ROOT / f"tools/parity/queries-{st}.txt").read_text(encoding="utf-8").splitlines()):
            if not l.strip() or l.startswith("#"):
                continue
            q, _, dist = l.partition("\t")
            w = q.split()
            if len(w) < 2 or any(c in q for c in '*?%|~"/()-+'):
                continue
            qs.append((st, n, q, w, dist.strip()))
    res = {"same_section": [], "cross_line": []}
    for st, n, q, w, dist in qs:
        tag = f"{st} q{n} ({len(w)} words)"
        # ---- sameSection
        ss, _, ms = search(d, ix, {"query": q, "same_section": True})
        # a word of MAX_TERM_BYTES (128) bytes or more is dropped from the query (not indexed),
        # so it places no sameSection constraint: leave it out of the recount
        kept = [x for x in w if len(x.encode("utf-8")) < 128] or w
        per = [set(h[1] for h in search(d, ix, {"query": x})[0]) for x in kept]
        inline = set(h[1] for h in search(d, ix, {"query": q})[0])
        union = set().union(*per)
        info = line_info(db, list(union))
        def sec(l):
            if l >= R or l not in info:
                return ("line", l)
            b, _, toc = info[l]
            return ("line", l) if toc is None else (b, toc)
        secs_per = [set(sec(l) for l in p) for p in per]
        full = set.intersection(*secs_per)
        expected = set(l for l in union if sec(l) in full)
        got = set(h[1] for h in ss)
        row = dict(set=st, n=n, words=len(w), ss_hits=len(got), expected=len(expected), inline=len(inline),
                   missing=len(expected - got), extra=len(got - expected), inline_not_in_ss=len(inline - got),
                   ms=round(ms), missing_ids=sorted(expected - got), extra_ids=sorted(got - expected))
        res["same_section"].append(row)
        print(f"SS {tag}: ours {len(got)} expected {len(expected)} missing {row['missing']} extra {row['extra']} inline {len(inline)} inline-not-in-ss {row['inline_not_in_ss']} ({ms:.0f} ms)", flush=True)
        # ---- cross-line (Otzaria advanced mode: ordered, gap = distance or 1000 as hitcount.py; also no distance unordered=adjacent)
        gap = int(dist) if dist else 1000
        prox = {"mode": "ordered", "gaps": [gap] * (len(w) - 1)}
        a, _, _ = search(d, ix, {"query": q, "proximity": prox})
        b, nx, msx = search(d, ix, {"query": q, "proximity": prox, "cross_line": True}, nexts=True)
        aset = set(h[1] for h in a)
        cross = [(h[1], x) for h, x in zip(b, nx) if x != 2 ** 64 - 1]
        lost = aset - set(h[1] for h in b)
        ci = line_info(db, [l for p in cross for l in p])
        same_book = consec = gapped = 0
        gapped_pairs, other_book = [], []
        for l, x in cross:
            if l in ci and x in ci:
                if ci[l][0] == ci[x][0]:
                    same_book += 1
                    if ci[x][1] == ci[l][1] + 1: consec += 1
                    else: gapped += 1; gapped_pairs.append([l, x])
                else:
                    other_book.append([l, x])
        cross_in_inline = sum(1 for l, _ in cross if l in aset)
        row = dict(set=st, n=n, gap=gap, inline=len(a), cross_on=len(b), cross_only=len(cross), lost_inline=len(lost),
                   cross_checked=sum(1 for l, x in cross if l in ci and x in ci), same_book=same_book,
                   consecutive=consec, gapped=gapped, cross_flag_on_inline=cross_in_inline, ms=round(msx),
                   lost_inline_ids=sorted(lost), other_book_pairs=other_book, gapped_pairs=gapped_pairs)
        res["cross_line"].append(row)
        print(f"XL {tag}: inline {len(a)} with-cross {len(b)} cross-only {len(cross)} lost {len(lost)} same-book {same_book} consec {consec} gapped {gapped} flag-on-inline {cross_in_inline} ({msx:.0f} ms)", flush=True)
    bad = [f"SS {r['set']} q{r['n']}" for r in res["same_section"] if r["missing"] or r["extra"]]
    bad += [f"XL {r['set']} q{r['n']}" for r in res["cross_line"] if r["lost_inline"] or r["other_book_pairs"]]
    res["failures"] = bad
    Path(outp).write_text(json.dumps(res, indent=1), encoding="utf-8")
    print(f"sameSection: {len(res['same_section'])} queries, cross-line: {len(res['cross_line'])} queries, "
          f"failures: {len(bad)}" + (" (" + ", ".join(bad) + ")" if bad else ""), flush=True)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main(*sys.argv[1:5]))
