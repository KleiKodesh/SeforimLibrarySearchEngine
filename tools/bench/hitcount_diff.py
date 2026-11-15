"""Line-level diff of the Otzaria hit-count rows: ours vs the Otzaria driver, per query. ASCII only.

usage: python -I -X utf8 tools/bench/hitcount_diff.py <native lib> <ours pos index> <otzaria driver>
       <otzaria index> <seforim.db> <otz-hitcount.json> <out json> [sample per side, default 8] [rows: set:n,...]

For every non-equal row of otz-hitcount.json (or the listed rows): runs Otzaria with the row's mode
(as otz_report.py) for every hit, maps its (book, segment) to line ids, runs ours as hitcount.py, maps
our footnote hits (virtual notes lines, id >= 2^52) to their parent line (Otzaria rehydrates a
line's notes into the line), and prints:
  ours raw / ours distinct parent lines / Otzaria / both / ours-only / otz-only, and how many of our
  hits are notes lines whose parent is also hit (Otzaria counts that line once);
  per side, sampled line ids with per-line features: has notes, tags, quotes, maqaf, length.
The out json keeps every differing line id per row."""
import ctypes as C, json, re, sqlite3, subprocess, sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
sys.path.insert(0, str(HERE))
import hitcount as H  # noqa: E402
import otz_report as O  # noqa: E402

R = 1 << 52


def ours_hits(d, ix, req):
    js = json.dumps(req, ensure_ascii=False).encode()
    s = d.se_search(ix, js, len(js))
    if not s:
        raise RuntimeError((d.se_last_error() or b"").decode("utf-8", "replace").encode("ascii", "replace").decode())
    n = d.se_search_hit_count(s)
    buf = (C.c_uint64 * (3 * max(n, 1)))()
    d.se_search_copy_hits(s, 0, n, buf)
    d.se_search_close(s)
    return [buf[3 * i + 1] for i in range(n)]


class Lines:
    def __init__(self, db):
        self.db = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
        self.by_book = {}
        self.has_note = None

    def line_id(self, book, seg):
        m = self.by_book.get(book)
        if m is None:
            m = {li: i for i, li in self.db.execute("SELECT id, lineIndex FROM line WHERE bookId=?", (book,))}
            self.by_book[book] = m
        return m.get(seg)

    def notes(self):
        if self.has_note is None:
            try:
                self.has_note = {r[0] for r in self.db.execute("SELECT DISTINCT lineId FROM line_note")}
            except sqlite3.Error:
                self.has_note = set()
        return self.has_note


def features(text):
    if text is None:
        return "no-text"
    f = []
    if "<" in text: f.append("tag")
    if "<sup" in text: f.append("sup")
    if re.search(r"[א-ת][\"'׳״][א-ת]?", text): f.append("quote")
    if "־" in text: f.append("maqaf")
    if "&" in text: f.append("amp")
    if re.search(r"[֑-ׇ]", text): f.append("marks")
    f.append(f"len{len(text)}")
    return ",".join(f)


def main(lib, index, drv, otzix, db, hc, outp, per="8", only=""):
    per = int(per)
    want = set(only.split(",")) if only else None
    d = H.load(lib)
    d.se_search_copy_hits.restype = C.c_uint64
    d.se_search_copy_hits.argtypes = [C.c_void_p, C.c_uint64, C.c_uint64, C.POINTER(C.c_uint64)]
    ix = d.se_index_open(index.encode(), len(index.encode()))
    if not ix:
        raise SystemExit("index open failed")
    L = Lines(db)
    sys.path.insert(0, str(ROOT / "tools/parity"))
    from explain_pos_diffs import Texts, load as eload, locate, session  # noqa: E402
    de = eload(lib)
    ixe = de.se_index_open(index.encode(), len(index.encode()))
    T = Texts(db)
    rows = json.loads(Path(hc).read_text(encoding="utf-8"))["sets"]
    res = []
    offset = None
    for st, rs in rows.items():
        qs = [l for l in (ROOT / f"tools/parity/queries-{st}.txt").read_text(encoding="utf-8").splitlines()
              if l.strip() and not l.startswith("#")]
        for r in rs:
            key = f"{st}:{r['n']}"
            if (want and key not in want) or (not want and r["ours"] == r["otz"]):
                continue
            q, _, dist = qs[r["n"]].partition("\t")
            args, _ = O.translate(q, dist.strip())
            req = H.translate(H.reduce_paired(d, q), dist.strip())
            if args is None or req is None:
                continue
            p = subprocess.run([drv, "search", db, otzix, "--limit", "10000000"] + args, capture_output=True)
            if p.returncode:
                print(f"== {key}: Otzaria driver exit {p.returncode}: "
                      + p.stderr.decode("utf-8", "replace")[-300:].encode("ascii", "replace").decode(), flush=True)
                continue
            segs = []
            for ln in p.stdout.decode("utf-8", "replace").splitlines():
                try:
                    j = json.loads(ln)
                except ValueError:
                    continue
                segs.append((j.get("book"), j.get("segment")))
            ours_raw = ours_hits(d, ix, req)
            parent = lambda x: ((x - R) >> 16) if x >= R else x
            ours = set(parent(x) for x in ours_raw)
            if offset is None and segs:
                # segment -> lineIndex: pick the offset that overlaps ours most (checked once)
                ov = {o: len(ours & {L.line_id(b, s + o) for b, s in segs[:2000] if s is not None}) for o in (0, -1, 1)}
                best = max(ov, key=ov.get)
                if ov[best]:
                    offset = best
                print(f"segment -> lineIndex offset {best} (overlaps {ov})", flush=True)
            otz, miss = set(), 0
            for b_, s_ in segs:
                lid = L.line_id(b_, s_ + (offset or 0)) if s_ is not None else None
                if lid is None:
                    miss += 1
                else:
                    otz.add(lid)
            real = set(x for x in ours_raw if x < R)
            note_dups = sum(1 for x in ours_raw if x >= R and parent(x) in real)
            oo, zo = sorted(ours - otz), sorted(otz - ours)
            notes = L.notes()
            se = session(de, ixe, req)
            def show(ids, loc=False):
                out = []
                for lid in ids[:per]:
                    t = T.get(lid)
                    lc = ""
                    if loc and t is not None:
                        lc = "ours-locates," if locate(de, se, t) is not None else "ours-no-match,"
                    out.append(f"{lid}[{'notes,' if lid in notes else ''}{lc}{features(t)}]")
                return " ".join(out)
            print(f"== {key} mode {args[1]}: ours raw {len(ours_raw)} lines {len(ours)} (note hits on a hit line: {note_dups}) "
                  f"otz {len(otz)} (unmapped {miss}) both {len(ours & otz)} ours-only {len(oo)} otz-only {len(zo)}", flush=True)
            print(f"   ours-only: {show(oo)}\n   otz-only:  {show(zo, True)}", flush=True)
            de.se_search_close(se)
            res.append(dict(key=key, ours_raw=len(ours_raw), ours_lines=len(ours), note_dups=note_dups, otz=len(otz),
                            unmapped=miss, ours_only=oo, otz_only=zo,
                            ours_only_notes=sum(1 for x in oo if x in notes), otz_only_notes=sum(1 for x in zo if x in notes)))
    Path(outp).write_text(json.dumps(res), encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main(*sys.argv[1:]))
